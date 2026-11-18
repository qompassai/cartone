//! The real binary over stdio: identity, tool surface, and a
//! `/bin/true` test-route job completing end to end.
//!
//! Sessions open with the modern `server/discover` handshake and
//! every request carries its protocol version in `params._meta`;
//! the legacy `initialize` handshake is refused by the era guard.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

const DEADLINE: Duration = Duration::from_secs(20);

const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"test","version":"0"},"io.modelcontextprotocol/clientCapabilities":{}}"#;

fn discover(id: u64) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"server/discover","params":{{{META}}}}}"#)
}

fn request(id: u64, method: &str, params: &str) -> String {
    let params = if params.is_empty() {
        META.to_string()
    } else {
        format!("{params},{META}")
    };
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{{params}}}}}"#)
}

fn call(id: u64, name: &str, arguments: Value) -> String {
    request(
        id,
        "tools/call",
        &format!(r#""name":"{name}","arguments":{arguments}"#),
    )
}

/// An interactive session: requests are written one at a time and
/// each response line is read with a deadline, so a job can be
/// polled while the same server process keeps running it.
struct Session {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
}

impl Session {
    fn spawn(data_dir: &std::path::Path, routes_file: &std::path::Path) -> Session {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cartone"))
            .env("CARTONE_DATA_DIR", data_dir)
            .env("CARTONE_ROUTES_FILE", routes_file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn cartone");
        let stdin = child.stdin.take().expect("stdin");
        let stdout: ChildStdout = child.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Session {
            child,
            stdin,
            lines: rx,
        }
    }

    fn send(&mut self, frame: &str) {
        writeln!(self.stdin, "{frame}").expect("write request");
        self.stdin.flush().expect("flush");
    }

    /// Read lines until the response for `id` arrives.
    fn await_id(&mut self, id: u64) -> Value {
        loop {
            let line = self
                .lines
                .recv_timeout(DEADLINE)
                .unwrap_or_else(|_| panic!("no response for id {id} within {DEADLINE:?}"));
            let value: Value = serde_json::from_str(&line).expect("stdout is JSON-RPC only");
            if value["id"] == id {
                return value;
            }
        }
    }

    fn finish(mut self) {
        drop(self.stdin);
        let started = std::time::Instant::now();
        loop {
            match self.child.try_wait().expect("poll") {
                Some(status) => {
                    assert!(status.success(), "cartone exited with {status}");
                    return;
                }
                None => {
                    assert!(
                        started.elapsed() < DEADLINE,
                        "cartone did not exit within {DEADLINE:?}"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
}

fn result_text(response: &Value) -> Value {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    serde_json::from_str(text).expect("tool result is JSON")
}

fn write_routes(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("routes.toml");
    std::fs::write(
        &path,
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
    .expect("write routes");
    path
}

#[test]
fn identity_tools_and_true_route_lifecycle() {
    let data = tempfile::tempdir().expect("tempdir");
    let routes = write_routes(data.path());
    let mut session = Session::spawn(data.path(), &routes);

    session.send(&discover(1));
    let info = &session.await_id(1)["result"]["_meta"]["io.modelcontextprotocol/serverInfo"];
    assert_eq!(info["name"], "cartone");
    assert_eq!(info["title"], "Cartone");
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));

    session.send(&request(2, "tools/list", ""));
    let tools = session.await_id(2)["result"]["tools"]
        .as_array()
        .expect("tools")
        .clone();
    let mut names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("name"))
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "cancel_job",
            "get_job",
            "ledger_summary",
            "list_jobs",
            "list_routes",
            "submit_job"
        ]
    );

    // The test route runs /bin/true end to end.
    session.send(&call(
        3,
        "submit_job",
        json!({"route": "true-route", "lane": "sfw", "kind": "image", "units": 2}),
    ));
    let submitted = result_text(&session.await_id(3));
    let job_id = submitted["job_id"].as_str().expect("job id").to_string();
    assert_eq!(submitted["cost"], 4.0);

    let mut status = String::new();
    for n in 0..100 {
        let id = 10 + n;
        session.send(&call(id, "get_job", json!({"job_id": job_id})));
        let record = result_text(&session.await_id(id));
        status = record["status"].as_str().expect("status").to_string();
        if status == "completed" {
            assert_eq!(record["exit_code"], 0);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(status, "completed", "the /bin/true job must complete");

    // The gate over the wire: nsfw on an sfw-only route is a
    // ledgered denial, returned as the job's record.
    session.send(&call(
        200,
        "submit_job",
        json!({"route": "hosted-x", "lane": "nsfw", "kind": "video", "units": 1}),
    ));
    let denied = result_text(&session.await_id(200));
    assert_eq!(denied["status"], "denied");
    assert_eq!(denied["cost"], 0.0);

    session.send(&call(201, "ledger_summary", json!({})));
    let summary = result_text(&session.await_id(201));
    assert_eq!(summary["totals"]["jobs"], 2);
    assert_eq!(summary["totals"]["completed"], 1);
    assert_eq!(summary["totals"]["denied"], 1);
    assert_eq!(summary["totals"]["total_cost"], 4.0);

    session.finish();

    // The ledger on disk carries metadata only, and the fold
    // survives the process.
    let ledger_text =
        std::fs::read_to_string(data.path().join("ledger.jsonl")).expect("ledger file");
    assert!(ledger_text.contains("\"true-route\""));
    assert!(
        !ledger_text.contains("prompt"),
        "no prompt content in the ledger"
    );
}

#[test]
fn legacy_initialize_is_refused_and_the_server_stays_up() {
    let data = tempfile::tempdir().expect("tempdir");
    let routes = write_routes(data.path());
    let mut session = Session::spawn(data.path(), &routes);
    session.send(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
    );
    let refusal = session.await_id(1);
    assert_eq!(refusal["error"]["code"], -32022);
    assert_eq!(refusal["error"]["data"]["supported"], json!(["2026-07-28"]));
    session.send(&discover(2));
    let info = &session.await_id(2)["result"]["_meta"]["io.modelcontextprotocol/serverInfo"];
    assert_eq!(info["name"], "cartone");
    session.finish();
}
