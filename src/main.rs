use roro::{
    auth::Auth,
    cli::{self, OperationMode},
    config::Config,
    error::{AppError, Result},
    http,
    store::Store,
};
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "roro=info".into()),
        )
        .init();
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    let options = cli::parse(std::env::args().skip(1))?;
    let serve = match options.mode {
        OperationMode::Help => {
            print!("{}", cli::HELP);
            return Ok(());
        }
        OperationMode::Version => {
            println!("roro {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        OperationMode::Serve(o) => o,
    };
    let config = Config::load(&serve.config_path)?;
    let root = config.data_root.clone();
    let store = roro::fs::blocking(move || Store::open(&root)).await?;
    let auth = Auth::new(config.clone(), store.disk.clone()).await?;
    let tls = roro::tls::load(&config, store.disk.clone()).await?;
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await?
        .into_std()?;
    let handle = axum_server::Handle::new();
    let state = http::AppState::new(store.clone(), auth);
    let app = http::router(state.clone());
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let gc_state = state.clone();
    let gc = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = stop_rx.changed() => break,
                _ = interval.tick() => {
                    let store = gc_state.store.clone();
                    match roro::fs::blocking(move || roro::gc::run(&store)).await {
                        Ok(report) => {
                            gc_state.metrics.gc_objects.fetch_add(report.removed_objects, std::sync::atomic::Ordering::Relaxed);
                            tracing::info!(?report, "garbage collection finished");
                        }
                        Err(error) => {
                            gc_state.metrics.gc_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            tracing::error!(%error, "garbage collection aborted; content retained");
                        }
                    }
                }
            }
        }
    });
    tracing::info!(listen=%config.listen,"HTTPS registry ready; all pulls are anonymous");
    let shutdown_handle = handle.clone();
    let shutdown = tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => (),
            _ = term.recv() => (),
        };
        let _ = stop_tx.send(true);
        shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(60)));
    });
    let result = axum_server::from_tcp_rustls(listener, tls)?
        .handle(handle)
        .serve(app.into_make_service())
        .await
        .map_err(AppError::internal);
    shutdown.abort();
    // Finish any in-flight GC transaction before dropping the runtime.
    if result.is_err() {
        gc.abort();
    } else {
        let _ = gc.await;
    }
    result
}
