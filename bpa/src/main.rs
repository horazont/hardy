use hardy_bpa_core::*;
use log_err::*;

mod cache;
mod cla_registry;
mod ingress;
mod logger;
mod services;
mod settings;

// Buildtime info
mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

fn init_metadata_storage(
    config: &config::Config,
) -> Result<std::sync::Arc<impl storage::MetadataStorage>, anyhow::Error> {
    #[cfg(feature = "sqlite-storage")]
    hardy_sqlite_storage::Storage::init(&config.get_table(hardy_sqlite_storage::CONFIG_KEY)?)
}

fn init_bundle_storage(
    config: &config::Config,
) -> Result<std::sync::Arc<impl storage::BundleStorage>, anyhow::Error> {
    #[cfg(feature = "localdisk-storage")]
    hardy_localdisk_storage::Storage::init(&config.get_table(hardy_localdisk_storage::CONFIG_KEY)?)
}

fn listen_for_cancel(
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    let mut term_handler =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .log_expect("Failed to register signal handlers");

    task_set.spawn(async move {
        tokio::select! {
            _ = term_handler.recv() =>
                {
                    // Signal stop
                    log::info!("{} stopping...", built_info::PKG_NAME);
                    cancel_token.cancel();
                }
            _ = cancel_token.cancelled() => {}
        }
    });
}

#[tokio::main]
async fn main() {
    // load config
    let Some(config) = settings::init() else {
        return;
    };

    // Init logger
    logger::init(&config);
    log::info!("{} starting...", built_info::PKG_NAME);

    // Init pluggable storage engines
    let cache = cache::Cache::new(
        &config,
        init_metadata_storage(&config).log_expect("Failed to initialize metadata store"),
        init_bundle_storage(&config).log_expect("Failed to initialize bundle store"),
    );

    // Prepare for graceful shutdown
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let mut task_set = tokio::task::JoinSet::new();
    listen_for_cancel(&mut task_set, cancel_token.clone());

    // Create a new ingress - this can take a while
    let ingress = ingress::Ingress::init(&config, cache, &mut &mut task_set, cancel_token.clone())
        .await
        .log_expect("Failed to initialize ingress");

    // Init gRPC services
    if !cancel_token.is_cancelled() {
        services::init(&config, ingress, &mut task_set, cancel_token.clone())
            .log_expect("Failed to start gRPC services");
    }

    // Wait for all tasks to finish
    if !cancel_token.is_cancelled() {
        log::info!("{} started", built_info::PKG_NAME);
    }
    while let Some(r) = task_set.join_next().await {
        r.log_expect("Task terminated unexpectedly")
    }

    log::info!("{} stopped", built_info::PKG_NAME);
}
