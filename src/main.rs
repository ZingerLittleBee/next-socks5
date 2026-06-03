//! Binary entry point: parse CLI, build shared state, bind the listener, run
//! the server, and drive either the TUI dashboard or a headless stdout loop.
//! Both modes request a graceful shutdown via the `watch` channel so the
//! accept loop stops and in-flight connections drain (bounded by a grace
//! timeout).

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};

use next_socks5::config::{AuthMethod, Cli, Config};
use next_socks5::metrics::{format_event, Event, Metrics};
use next_socks5::server;
#[cfg(feature = "tui")]
use next_socks5::tui;

/// Bound on how long we wait for in-flight connections to drain on shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() {
    // 1. Parse CLI and load config; a config error is fatal.
    let cli = Cli::parse();
    let cfg = match Config::load(&cli) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(1);
        }
    };

    // 2. Build shared state: metrics, the event bus, and the shutdown channel.
    let metrics = Metrics::new();
    let (events_tx, events_rx) = broadcast::channel::<Event>(1024);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // 3. Bind the listener up front so a bind failure is reported before any
    //    UI takes over the terminal.
    let listener = match TcpListener::bind(&cfg.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to bind {}: {e}", cfg.listen);
            std::process::exit(1);
        }
    };
    let local = listener.local_addr().ok();
    let listen_str = local
        .map(|a| a.to_string())
        .unwrap_or_else(|| cfg.listen.clone());
    let auth_str = match cfg.auth.method {
        AuthMethod::None => "none",
        AuthMethod::Password => "password",
    };

    // 4. Spawn the server accept loop.
    let server_handle = tokio::spawn(server::run(
        listener,
        cfg.clone(),
        metrics.clone(),
        events_tx.clone(),
        shutdown_rx.clone(),
    ));

    // 5. Run the chosen front end. Each branch is responsible for flipping the
    //    shutdown channel to `true` before returning.
    #[cfg(feature = "tui")]
    {
        if cli.no_tui {
            run_headless(&listen_str, auth_str, events_rx, &shutdown_tx).await;
        } else {
            // Seed the TUI log with startup info before the dashboard takes over.
            let _ = events_tx.send(Event::Log(format!("listening on {listen_str}")));
            let _ = events_tx.send(Event::Log(format!("auth: {auth_str}")));
            let _ = events_tx.send(Event::Log("press q to quit".to_string()));

            let source: Arc<dyn next_socks5::metrics::MetricsSource> = metrics.clone();
            if let Err(e) = tui::run(
                source,
                events_rx,
                shutdown_tx.clone(),
                shutdown_rx.clone(),
                Some(listen_str.clone()),
            )
            .await
            {
                eprintln!("tui error: {e}");
            }
        }
    }
    // Without the `tui` feature the dashboard is unavailable, so we always run
    // headless regardless of `--no-tui`. Bind the otherwise-unused values so the
    // headless-only build compiles without warnings.
    #[cfg(not(feature = "tui"))]
    {
        let _ = (&cli.no_tui, &metrics, &shutdown_rx);
        run_headless(&listen_str, auth_str, events_rx, &shutdown_tx).await;
    }

    // 6. Ensure shutdown is requested (idempotent) and wait for the server to
    //    drain, bounded by the grace period so we never hang forever.
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, server_handle).await;
    println!("shutdown complete");
}

/// Headless mode: print a startup banner, stream events to stdout, and wait for
/// Ctrl-C to request shutdown.
async fn run_headless(
    listen_str: &str,
    auth_str: &str,
    mut events_rx: broadcast::Receiver<Event>,
    shutdown_tx: &watch::Sender<bool>,
) {
    println!("next-socks5 listening on {listen_str} (auth: {auth_str}, headless)");

    // Drain the event bus to stdout on a background task. It ends when the
    // sender is dropped (server gone) or the channel closes.
    let drain = tokio::spawn(async move {
        loop {
            match events_rx.recv().await {
                Ok(ev) => println!("{}", format_event(&ev)),
                Err(RecvError::Lagged(n)) => println!("dropped {n} events"),
                Err(RecvError::Closed) => break,
            }
        }
    });

    // Wait for Ctrl-C, then request a graceful shutdown.
    if let Err(e) = tokio::signal::ctrl_c().await {
        eprintln!("failed to listen for ctrl-c: {e}");
    }
    let _ = shutdown_tx.send(true);

    // The drain task will exit once the server drops its event senders.
    let _ = drain.await;
}
