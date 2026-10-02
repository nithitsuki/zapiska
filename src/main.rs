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
            // Observe the result. The supervisor publishes the exit on
            // `wm_worker_exit` for the shutdown decision below, but the handle
            // is the authoritative answer and discarding it is the defect this
            // change set exists to remove. A timeout is not an exit: it means
            // the worker did not stop in time, so it is reported separately
            // rather than being confused with a clean stop.
            match tokio::time::timeout(
                zapiska::worker::DRAIN_TIMEOUT + std::time::Duration::from_secs(5),
                handle,
            )
            .await
            {
                Ok(Ok(exit)) => tracing::info!(?exit, "webmention worker stopped"),
                // The supervisor itself failing is a worker failure: nothing
                // published, so the exit code below may read `None` and exit 0.
                // Log it loudly so the failure is never silent.
                Ok(Err(join_err)) => tracing::error!(
                    error = %join_err,
                    panicked = join_err.is_panic(),
                    "webmention worker supervisor could not be joined"
                ),
                Err(_) => tracing::warn!(
                    timeout_secs = (zapiska::worker::DRAIN_TIMEOUT
                        + std::time::Duration::from_secs(5))
                    .as_secs(),
                    "webmention worker did not stop within the drain bound"
                ),
            }
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
    //
    // `None` is a FAILURE here, not "still running": this code runs after
    // `serve` has returned, so the server is stopping either way, and a
    // `None` means the supervisor never published an outcome (it failed, or
    // the drain timed out). Treating it as success would restore the exact
    // silent failure this commit set removes.
    #[cfg(feature = "webmentions")]
    match worker_exit.borrow().clone() {
        Some(zapiska::worker::WorkerExit::Graceful) => {}
        Some(exit) => {
            tracing::error!(
                ?exit,
                "webmention worker died; exiting non-zero so the orchestrator restarts the process"
            );
            std::process::exit(1);
        }
        None => {
            tracing::error!(
                "webmention worker published no exit outcome; exiting non-zero so the                  orchestrator restarts the process"
            );
            std::process::exit(1);
        }
    }
}
