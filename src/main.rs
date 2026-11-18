//! cartone — MCP stdio server binary. Transport wiring only; the
//! logic lives in the `cartone` library crate.
//!
//! The server speaks the MCP modern era only (2026-07-28):
//! sessions open with `server/discover` and every request carries
//! its protocol version in `_meta` — there is no `initialize`
//! handshake. Legacy contact is refused at the stdio front door
//! (see `cartone::guard`) with the typed `-32022`
//! unsupported-version error.
//!
//! Startup is fail-closed: the data directory and route table
//! must resolve and validate before anything is served. When no
//! routes file exists yet, the built-in table (PLAN §2.3) is
//! written to `<data dir>/routes.toml` and used, so a first run
//! leaves an editable, explicit configuration behind.
//!
//! Usage: `cartone` serves; `cartone --version` prints the
//! version; `cartone --help` prints usage. Unknown arguments are
//! warned about and ignored.

#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::sync::Arc;

use cartone::broker::Broker;
use cartone::config;
use cartone::guard;
use cartone::server::CartoneServer;
use rmcp::ServiceExt;
use rmcp::transport::async_rw::AsyncRwTransport;
use tokio::sync::Mutex;

/// Duplex buffer between the era guard and the SDK service.
const DUPLEX_BUFFER_BYTES: usize = 1024 * 1024;

async fn run(broker: Broker) -> std::io::Result<()> {
    let (client_side, server_side) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
    let (server_read, server_write) = tokio::io::split(server_side);
    let (client_read, client_write) = tokio::io::split(client_side);
    let stdout: guard::SharedStdout = Arc::new(Mutex::new(tokio::io::stdout()));

    let ordering = guard::Ordering::new();
    let handler = CartoneServer::new(broker);
    let serve = tokio::spawn(async move {
        let transport = AsyncRwTransport::new_server(server_read, server_write);
        match handler.serve(transport).await {
            Ok(running) => running
                .waiting()
                .await
                .map(|_| ())
                .map_err(|e| std::io::Error::other(e.to_string())),
            // The client closed the connection before selecting a
            // lifecycle (a probe, a spawn check): bare EOF is a
            // clean end of session.
            Err(rmcp::service::ServerInitializeError::ConnectionClosed(_)) => Ok(()),
            Err(e) => Err(std::io::Error::other(e.to_string())),
        }
    });
    let mut pump = tokio::spawn(guard::pump(
        client_read,
        Arc::clone(&stdout),
        Arc::clone(&ordering),
    ));

    let guard_result = guard::run(tokio::io::stdin(), client_write, stdout, ordering).await;
    let serve_result = match serve.await {
        Ok(result) => result,
        Err(e) => Err(std::io::Error::other(e.to_string())),
    };
    if tokio::time::timeout(std::time::Duration::from_secs(2), &mut pump)
        .await
        .is_err()
    {
        pump.abort();
    }
    guard_result.and(serve_result)
}

/// Resolve configuration and open the broker. Any failure is a
/// refusal with the reason on stderr — no partial startup.
fn startup() -> Result<Broker, String> {
    let data_dir = config::resolve_data_dir(
        std::env::var(config::DATA_DIR_ENV).ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )?;
    let routes_path = config::resolve_routes_path(
        std::env::var(config::ROUTES_FILE_ENV).ok().as_deref(),
        &data_dir,
    );
    let routes = if routes_path.exists() {
        config::Routes::load(&routes_path).map_err(|e| e.to_string())?
    } else {
        let defaults = config::Routes::defaults();
        if let Some(parent) = routes_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let text = defaults.to_toml().map_err(|e| e.to_string())?;
        std::fs::write(&routes_path, text)
            .map_err(|e| format!("write {}: {e}", routes_path.display()))?;
        eprintln!(
            "cartone: wrote default route table to {}",
            routes_path.display()
        );
        defaults
    };
    Broker::open(routes, &data_dir).map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("cartone {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!(
                    "cartone {} — generation broker MCP (stdio, modern era 2026-07-28)\n\
                     \n\
                     Environment:\n\
                     \x20 CARTONE_DATA_DIR    data directory (default ~/.local/share/cartone)\n\
                     \x20 CARTONE_ROUTES_FILE  route table TOML (default <data dir>/routes.toml)",
                    env!("CARGO_PKG_VERSION")
                );
                return ExitCode::SUCCESS;
            }
            other => eprintln!("cartone: ignoring unknown arg {other}"),
        }
    }
    let broker = match startup() {
        Ok(broker) => broker,
        Err(e) => {
            eprintln!("cartone: refusing to start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("cartone: cannot start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(broker)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cartone: serve loop ended: {e}");
            ExitCode::FAILURE
        }
    }
}
