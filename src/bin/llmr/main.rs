//! `llmr`, an LLM router run as a container and managed over REST.
//!
//! ```text
//! llmr [serve]       run the gateway (the default)
//! llmr keygen        print a new master key for LLMR_MASTER_KEY
//! llmr healthcheck   exit 0 when the local gateway answers /healthz
//! ```
//!
//! Everything else is configured through the management API and kept in the database. The
//! process itself reads only its environment:
//!
//! | Variable | |
//! |---|---|
//! | `LLMR_MASTER_KEY` | Required. The key credentials are sealed with; `llmr keygen` makes one |
//! | `LLMR_DATA_DIR` | Where the database lives. `/var/lib/llmr`, a volume in the image |
//! | `LLMR_TOKEN` | Optional. Tokens callers must present, comma separated. Unset: no check |
//! | `LLMR_LISTEN` | Address and port. `0.0.0.0:8080` |
//! | `LLMR_MAX_BODY_MB` | Largest request body. `32` |
//! | `RUST_LOG`, `LLMR_LOG_FORMAT` | Log filter, and `json` for one object per line |

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod crypto;
mod error;
mod gateway;
mod manage;
mod media;
mod openai;
mod prices;
mod records;
mod server;
mod store;
mod usage;

use crypto::MasterKey;
use gateway::{Gateway, Live, Snapshot};
use server::AppState;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use store::{Db, Store};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const USAGE: &str = "usage: llmr [serve|keygen|healthcheck]";

fn listen_address() -> String {
    std::env::var("LLMR_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["serve"] => {
            init_logging();
            serve().await
        }
        ["keygen"] => {
            println!("{}", MasterKey::generate());
            ExitCode::SUCCESS
        }
        ["healthcheck"] => healthcheck(&listen_address()).await,
        ["--version" | "-V"] => {
            println!("llmr {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        ["--help" | "-h"] => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => fail(USAGE),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("llmr: {message}");
    ExitCode::FAILURE
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("LLMR_LOG_FORMAT").is_ok_and(|f| f.eq_ignore_ascii_case("json"));
    // Colour only for a person at a terminal. `docker logs` and a log collector get plain
    // text, rather than escape codes around every field.
    let colour = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(colour);
    if json {
        builder.json().init();
    } else {
        builder.init();
    }
}

/// Tokens callers must present, from `LLMR_TOKEN`. Empty means no check.
fn tokens() -> Vec<String> {
    std::env::var("LLMR_TOKEN")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(String::from)
        .collect()
}

/// Says which routes cannot be used, then asks every usable route whether it is reachable.
///
/// Reports and does not prune: a denied route is rested by its breaker and still retried
/// later, and an unknown one is left exactly as it was.
async fn survey(gateway: Arc<Gateway>) {
    for (provider, why) in gateway.problems() {
        tracing::warn!(provider = %provider, problem = %why, "provider not built");
    }
    for served in gateway.served() {
        for (route, why) in &served.unavailable {
            tracing::warn!(model = %served.name, route = %route, why = %why, "route unavailable");
        }
        for (route, access) in served.router.preflight().await {
            if access.is_denied() {
                tracing::warn!(model = %served.name, route = %route, access = %access, "preflight");
            } else {
                tracing::info!(model = %served.name, route = %route, access = %access, "preflight");
            }
        }
    }
}

async fn serve() -> ExitCode {
    let key = match std::env::var("LLMR_MASTER_KEY") {
        Ok(text) if !text.trim().is_empty() => match MasterKey::from_base64(&text) {
            Ok(key) => key,
            Err(e) => return fail(&e),
        },
        _ => {
            return fail(
                "LLMR_MASTER_KEY is not set. It seals every stored credential; make one with `docker run --rm ghcr.io/bircex/llmr keygen` and keep a copy somewhere safe: without it the stored credentials cannot be read",
            )
        }
    };

    let dir =
        PathBuf::from(std::env::var("LLMR_DATA_DIR").unwrap_or_else(|_| "/var/lib/llmr".into()));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return fail(&format!(
            "cannot create the data directory {}: {e}",
            dir.display()
        ));
    }
    let path = dir.join("llmr.db");
    let store = match Store::open(&path, key) {
        Ok(store) => store,
        Err(e) => return fail(&format!("{}: {e}", path.display())),
    };
    let gateway = match Snapshot::read(&store) {
        Ok(snapshot) => Gateway::build(&snapshot),
        Err(e) => return fail(&format!("{}: {e}", path.display())),
    };

    let keys = tokens();
    if keys.is_empty() {
        tracing::warn!(
            "LLMR_TOKEN is not set: anybody who can reach this port can call models and change providers. Keep the port on a network only your panel can reach"
        );
    }
    let max_body_mb: usize = std::env::var("LLMR_MAX_BODY_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);

    let db = Db::new(store);
    let (recorder, writer) = usage::Recorder::start(db.clone());
    let state = Arc::new(AppState {
        live: Live::new(gateway),
        db,
        keys,
        started: std::time::Instant::now(),
        recorder,
        prices: prices::Sync::new(prices::Config::from_env()),
    });
    let app = server::app(state.clone(), max_body_mb * 1024 * 1024);

    let listen = listen_address();
    let listener = match tokio::net::TcpListener::bind(&listen).await {
        Ok(listener) => listener,
        Err(e) => return fail(&format!("cannot listen on {listen}: {e}")),
    };
    let current = state.live.current();
    tracing::info!(
        listen = %listen,
        data = %path.display(),
        route_sets = current.served().count(),
        models = current.enabled().count(),
        version = env!("CARGO_PKG_VERSION"),
        "llmr is serving"
    );

    // In the background, so a slow vendor does not hold up the port opening. Free: every
    // provider answers from its model list, never from a billable call.
    tokio::spawn(survey(current));
    // Prices from the public list, on its schedule. Never on the request path, and a failure
    // leaves the last good prices in place.
    tokio::spawn(prices::keep_current(state.clone()));

    let served = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await;

    // The last recorder goes with the state; the writer then writes what is queued and ends.
    // Bounded, so a stuck disk cannot keep a container from stopping.
    drop(state);
    if tokio::time::timeout(std::time::Duration::from_secs(5), writer)
        .await
        .is_err()
    {
        tracing::warn!("usage rows still queued at shutdown were not all written");
    }

    match served {
        Ok(()) => {
            tracing::info!("stopped");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("server error: {e}")),
    }
}

/// Resolves on Ctrl-C, or on SIGTERM, which is what `docker stop` sends.
async fn shutdown() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {},
        () = terminate => {},
    }
    tracing::info!("shutting down, finishing requests in flight");
}

/// Asks the local gateway for `/healthz`.
///
/// Written against a bare socket so the image needs no `curl`. Connects to loopback on the
/// configured port whatever address the server is bound to.
async fn healthcheck(listen: &str) -> ExitCode {
    let port = listen.rsplit(':').next().unwrap_or("8080");
    let address = format!("127.0.0.1:{port}");
    let attempt = async {
        let mut stream = TcpStream::connect(&address).await?;
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).await?;
        Ok::<_, std::io::Error>(reply)
    };
    match tokio::time::timeout(std::time::Duration::from_secs(3), attempt).await {
        Ok(Ok(reply)) if reply.starts_with(b"HTTP/1.1 200") => ExitCode::SUCCESS,
        Ok(Ok(_)) => fail("healthz did not answer 200"),
        Ok(Err(e)) => fail(&format!("{address}: {e}")),
        Err(_) => fail(&format!("{address}: no answer within 3s")),
    }
}
