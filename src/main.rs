use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;
use zapiska::config::Config;
use zapiska::http::{build_app, shutdown};
use zapiska::state::AppState;

#[tokio::main]
async fn main() {
    // `--version` / `-V` must not require configuration: operators check the
    // binary version before any `.env` exists.
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("zapiska {}", zapiska::APP_VERSION);
        return;
    }

    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config = Config::from_env().expect("failed to load configuration");
    info!(config = %config.redacted_display(), "server starting");

    let bind_addr = config.bind_addr;
    let database_path = config.database_path.clone();

    // All assembly (pool, migrations, gates, repo, notifier, worker) lives
    // in AppState::start — main only serves what it returns.
    let state = AppState::start(config).expect("failed to start");

    // Shutdown drain: after the server stops accepting connections, flush
    // open notification windows as final digests (same retry policy,
    // awaited) so a restart during a burst still alerts the admin.
    // Drained twice: handler senders die with the router at serve return,
    // but a webmention job already mid-flight can push during the first
    // drain — the second pass catches those stragglers. Residual
    // best-effort: a job completing after the second pass's checks is not
    // awaited (narrow: it must finish inside the second drain's tail).
    let drain_notifier = Arc::clone(&state.notifier);
    let drain_client = state.http_client.clone();
    // Webmention worker drain: clone the shutdown signal and the join handle
    // beside the notifier handles, before `build_app` consumes the state.
    #[cfg(feature = "webmentions")]
    let drain_worker = Arc::clone(&state.wm_worker);
    #[cfg(feature = "webmentions")]
    let worker_shutdown = state.wm_shutdown.clone();
    // Prompt worker-death observation: the supervisor publishes its exit
    // here. `main` stops serving as soon as this changes, so a dead consumer
    // is noticed during the run, not only when the process is asked to stop.
    #[cfg(feature = "webmentions")]
    let worker_exit = state.wm_worker_exit.clone();

    let app = build_app(state);

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("failed to bind address");
    // Graceful shutdown fires on the OS signal OR on webmention-worker death.
    // A dead consumer means the webmention queue is permanently dead: every
    // POST would 503 until a restart, so stop serving now.
    #[cfg(feature = "webmentions")]
    let shutdown_signal = {
        let mut worker_exit = worker_exit.clone();
        async move {
            // A worker that already died before this receiver was cloned must
            // not be missed: `changed` only fires on changes after the clone.
            if worker_exit.borrow().is_some() {
                return;
            }
            tokio::select! {
                _ = shutdown::shutdown_signal() => {}
                _ = worker_exit.changed() => {}
            }
        }
    };
    #[cfg(not(feature = "webmentions"))]
    let shutdown_signal = shutdown::shutdown_signal();

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal)
    .await
    .expect("server exited with error");
    // Worker first: signal the drain, then bounded-await the worker so its
    // queued jobs finish (and any notifications they push land) before the
    // notification drain flushes open windows.
    #[cfg(feature = "webmentions")]
    {
        let _ = worker_shutdown.send(true);
        // Take the handle out of the shared slot first, so the mutex guard
        // is dropped before the await below.
        let worker_handle = drain_worker.lock().expect("worker handle lock").take();
        if let Some(handle) = worker_handle {
            let _ = tokio::time::timeout(
                zapiska::worker::DRAIN_TIMEOUT + std::time::Duration::from_secs(5),
                handle,
            )
            .await;
        }
    }
    drain_notifier.drain(&drain_client).await;
    drain_notifier.drain(&drain_client).await;
    zapiska::state::release_db_lock(&database_path);

    // A dead consumer must fail the process. Both shipped orchestrators
    // restart on a non-zero exit: `docker-compose.yml` uses
    // `restart: unless-stopped` and `deploy/zapiska.service` uses
    // `Restart=on-failure`. Exiting non-zero turns a silently broken server
    // into a self-healing one. This runs AFTER the graceful sequence above
    // (worker drain, notification drain, `release_db_lock`) so the database
    // lock is released cleanly and the next start claims it fresh. A graceful
    // worker exit is the normal path and does not exit non-zero.
    #[cfg(feature = "webmentions")]
    if worker_exit
        .borrow()
        .as_ref()
        .is_some_and(|exit| !matches!(exit, zapiska::worker::WorkerExit::Graceful))
    {
        tracing::error!(
            exit = ?worker_exit.borrow().as_ref(),
            "webmention worker died; exiting non-zero so the orchestrator restarts the process"
        );
        std::process::exit(1);
    }
}
