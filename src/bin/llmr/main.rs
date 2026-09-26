//! `llmr`, the router as a service.
//!
//! ```text
//! llmr [serve] [--config PATH]    run the gateway (the default)
//! llmr check   [--config PATH]    read the configuration, build every provider, ask every
//!                                 route whether it is reachable, and exit
//! llmr healthcheck [--config PATH]  exit 0 when the local gateway answers /healthz
//! ```
//!
//! The configuration path is `--config`, else `LLMR_CONFIG`, else `llmr.toml`.
//! `LLMR_LISTEN` overrides the listen address, `RUST_LOG` the log filter, and
//! `LLMR_LOG_FORMAT=json` writes one JSON object per line for a log collector.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod config;
mod error;
mod gateway;
mod openai;
mod server;

use config::{AuthMode, Config};
use gateway::Gateway;
use server::AppState;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const USAGE: &str = "usage: llmr [serve|check|healthcheck] [--config PATH]";

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut command = None;
    let mut config_path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => match args.next() {
                Some(path) => config_path = Some(path),
                None => return fail(USAGE),
            },
            "--version" | "-V" => {
                println!("llmr {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "serve" | "check" | "healthcheck" if command.is_none() => command = Some(arg),
            _ => return fail(USAGE),
        }
    }
    let config_path = config_path
        .or_else(|| std::env::var("LLMR_CONFIG").ok())
        .unwrap_or_else(|| "llmr.toml".into());

    let config = match std::fs::read_to_string(&config_path)
        .map_err(|e| format!("{config_path}: {e}"))
        .and_then(|text| Config::parse(&text).map_err(|e| format!("{config_path}: {e}")))
    {
        Ok(config) => config,
        Err(e) => return fail(&e),
    };
    let listen = std::env::var("LLMR_LISTEN").unwrap_or_else(|_| config.server.listen.clone());

    match command.as_deref() {
        Some("healthcheck") => healthcheck(&listen).await,
        Some("check") => {
            init_logging();
            check(&config).await
        }
        _ => {
            init_logging();
            serve(config, listen).await
        }
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

/// The keys callers may present, or a reason the server must not start.
fn keys(config: &Config) -> Result<Vec<String>, String> {
    match config.server.auth {
        AuthMode::None => {
            tracing::warn!(
                "authentication is off: anybody who can reach this port can spend the \
                 providers' money"
            );
            Ok(Vec::new())
        }
        AuthMode::Keys => {
            let variable = &config.server.api_keys_env;
            let keys: Vec<String> = std::env::var(variable)
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(String::from)
                .collect();
            if keys.is_empty() {
                return Err(format!(
                    "{variable} holds no keys. Set it to one or more comma separated keys \
                     clients will present, or set `auth = \"none\"` under [server] for a \
                     network nobody else is on"
                ));
            }
            Ok(keys)
        }
    }
}

/// Says which routes can never be chosen, then asks every route whether it can be reached.
///
/// Reports and does not prune: a denied route is rested by its breaker and still retried
/// later, and an unknown one is left exactly as it was.
async fn survey(gateway: &Gateway, ask: bool) -> usize {
    let mut denied = 0;
    for served in gateway.served() {
        for route in served.router.unusable() {
            tracing::warn!(
                model = %served.name,
                route = %route,
                "the provider does not know this model, so no request can ever select it. \
                 Add a [[provider.model]] row for it or fix the name"
            );
        }
        if !ask {
            continue;
        }
        for (route, access) in served.router.preflight().await {
            if access.is_denied() {
                denied += 1;
                tracing::warn!(model = %served.name, route = %route, access = %access, "preflight");
            } else {
                tracing::info!(model = %served.name, route = %route, access = %access, "preflight");
            }
        }
    }
    denied
}

async fn check(config: &Config) -> ExitCode {
    let gateway = match Gateway::build(config) {
        Ok(gateway) => gateway,
        Err(e) => return fail(&e),
    };
    if let Err(e) = keys(config) {
        return fail(&e);
    }
    let unusable: usize = gateway.served().map(|s| s.router.unusable().len()).sum();
    let denied = survey(&gateway, true).await;
    if unusable + denied > 0 {
        return fail(&format!(
            "{unusable} route(s) can never be chosen and {denied} were denied; see above"
        ));
    }
    println!("configuration is sound");
    ExitCode::SUCCESS
}

async fn serve(config: Config, listen: String) -> ExitCode {
    let gateway = match Gateway::build(&config) {
        Ok(gateway) => gateway,
        Err(e) => return fail(&e),
    };
    let keys = match keys(&config) {
        Ok(keys) => keys,
        Err(e) => return fail(&e),
    };

    let state = Arc::new(AppState { gateway, keys });
    let app = server::app(state.clone(), config.server.max_body_mb * 1024 * 1024);

    let listener = match tokio::net::TcpListener::bind(&listen).await {
        Ok(listener) => listener,
        Err(e) => return fail(&format!("cannot listen on {listen}: {e}")),
    };
    tracing::info!(
        listen = %listen,
        models = state.gateway.served().count(),
        version = env!("CARGO_PKG_VERSION"),
        "llmr is serving"
    );

    // In the background, so a slow vendor does not hold up the port opening. Free: every
    // provider answers from its model list, never from a billable call.
    let preflight = config.server.preflight;
    let surveyed = state.clone();
    tokio::spawn(async move {
        survey(&surveyed.gateway, preflight).await;
    });

    match axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
    {
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
