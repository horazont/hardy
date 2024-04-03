use super::*;
use sha2::Digest;
use std::sync::Arc;

pub struct Cache {
    metadata_storage: Arc<dyn storage::MetadataStorage>,
    bundle_storage: Arc<dyn storage::BundleStorage>,
}

impl Clone for Cache {
    fn clone(&self) -> Self {
        Self {
            metadata_storage: self.metadata_storage.clone(),
            bundle_storage: self.bundle_storage.clone(),
        }
    }
}

fn init_metadata_storage(
    config: &config::Config,
    upgrade: bool,
) -> Result<std::sync::Arc<dyn storage::MetadataStorage>, anyhow::Error> {
    let default = if cfg!(feature = "sqlite-storage") {
        hardy_sqlite_storage::CONFIG_KEY
    } else {
        return Err(anyhow!("No default metadata storage engine, rebuild the package with at least one metadata storage engine feature enabled"));
    };

    let engine: String = settings::get_with_default(config, "metadata_storage", default)
        .map_err(|e| anyhow!("Failed to parse 'metadata_storage' config param: {}", e))?;

    let config = config.get_table(&engine).unwrap_or_default();

    log::info!("Using metadata storage: {}", engine);

    match engine.as_str() {
        #[cfg(feature = "sqlite-storage")]
        hardy_sqlite_storage::CONFIG_KEY => hardy_sqlite_storage::Storage::init(&config, upgrade),

        _ => Err(anyhow!("Unknown metadata storage engine {}", engine)),
    }
}

fn init_bundle_storage(
    config: &config::Config,
    _upgrade: bool,
) -> Result<std::sync::Arc<dyn storage::BundleStorage>, anyhow::Error> {
    let default = if cfg!(feature = "localdisk-storage") {
        hardy_localdisk_storage::CONFIG_KEY
    } else {
        return Err(anyhow!("No default bundle storage engine, rebuild the package with at least one bundle storage engine feature enabled"));
    };

    let engine: String = settings::get_with_default(config, "bundle_storage", default)
        .map_err(|e| anyhow!("Failed to parse 'bundle_storage' config param: {}", e))?;
    let config = config.get_table(&engine).unwrap_or_default();

    log::info!("Using bundle storage: {}", engine);

    match engine.as_str() {
        #[cfg(feature = "localdisk-storage")]
        hardy_localdisk_storage::CONFIG_KEY => hardy_localdisk_storage::Storage::init(&config),

        _ => Err(anyhow!("Unknown bundle storage engine {}", engine)),
    }
}

impl Cache {
    pub fn new(config: &config::Config, upgrade: bool) -> Self {
        // Init pluggable storage engines
        Self {
            metadata_storage: init_metadata_storage(config, upgrade)
                .log_expect("Failed to initialize metadata store"),
            bundle_storage: init_bundle_storage(config, upgrade)
                .log_expect("Failed to initialize bundle store"),
        }
    }

    pub async fn init(
        &self,
        ingress: ingress::Ingress,
        dispatcher: dispatcher::Dispatcher,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), anyhow::Error> {
        let self_cloned = self.clone();
        tokio::task::spawn_blocking(move || {
            log::info!("Starting cache reload...");

            // Bundle storage checks first
            self_cloned.bundle_storage_check(ingress, cancel_token.clone())?;

            // Now check the metadata storage for orphans
            if !cancel_token.is_cancelled() {
                self_cloned.metadata_storage_check(dispatcher, cancel_token)?;
            }

            log::info!("Cache reload complete");
            Ok::<(), anyhow::Error>(())
        })
        .await?
    }

    fn metadata_storage_check(
        &self,
        dispatcher: dispatcher::Dispatcher,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), anyhow::Error> {
        self.metadata_storage.check_orphans(&mut |bundle| {
            tokio::runtime::Handle::current().block_on(async {
                // The data associated with `bundle` has gone!
                dispatcher
                    .report_bundle_deletion(
                        &bundle,
                        dispatcher::BundleStatusReportReasonCode::DepletedStorage,
                    )
                    .await?;

                if let Some(metadata) = bundle.metadata {
                    // Delete it
                    self.metadata_storage.remove(&metadata.storage_name).await?;
                }
                Ok::<(), anyhow::Error>(())
            })?;

            // Just dumb poll the cancel token now - try to avoid mismatched state again
            Ok(!cancel_token.is_cancelled())
        })
    }

    fn bundle_storage_check(
        &self,
        ingress: ingress::Ingress,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), anyhow::Error> {
        self.bundle_storage.check_orphans(&mut |storage_name| {
            let r = tokio::runtime::Handle::current().block_on(async {
                // Check if the metadata_storage knows about this bundle
                if self
                    .metadata_storage
                    .confirm_exists(storage_name, None)
                    .await?
                {
                    return Ok::<bool, anyhow::Error>(true);
                }

                // Parse the bundle first
                let data = self.bundle_storage.load(storage_name).await?;
                let Ok((bundle, valid)) = bundle::parse((*data).as_ref()) else {
                    // Drop it... garbage
                    return Ok(false);
                };

                // Write to metadata or die trying
                let hash = sha2::Sha256::digest((*data).as_ref());
                self.metadata_storage
                    .store(storage_name, &hash, &bundle)
                    .await?;

                // Queue the new bundle for ingress processing
                ingress.enqueue_bundle(None, bundle, valid).await?;

                // true for keep
                Ok(true)
            })?;

            // Just dumb poll the cancel token now - try to avoid mismatched state again
            Ok((!cancel_token.is_cancelled()).then_some(r))
        })
    }

    pub async fn store(
        &self,
        data: Arc<Vec<u8>>,
    ) -> Result<(Option<bundle::Bundle>, bool), anyhow::Error> {
        // Start the write to bundle storage
        let write_result = self.bundle_storage.store(data.clone());

        // Parse the bundle in parallel
        let bundle_result = bundle::parse(&data);
        let hash = sha2::Sha256::digest(&*data);

        // Await the result of write to bundle storage
        let storage_name = write_result.await?;

        // Check parse result
        let (bundle, valid) = match bundle_result {
            Ok(r) => r,
            Err(e) => {
                // Parse failed badly, no idea who to report to
                log::info!("Bundle parsing failed: {}", e);

                // Remove from bundle storage
                let _ = self.bundle_storage.remove(&storage_name).await;
                return Ok((None, false));
            }
        };

        // Write to metadata store
        if let Err(e) = self
            .metadata_storage
            .store(&storage_name, &hash, &bundle)
            .await
        {
            // This is just bad, we can't really claim to have received the bundle,
            // so just cleanup and get out
            let _ = self.bundle_storage.remove(&storage_name).await;
            return Err(e);
        }

        // Return the parsed bundle
        Ok((Some(bundle), valid))
    }

    pub async fn remove(&self, bundle: bundle::Bundle) -> Result<(), anyhow::Error> {
        todo!()
    }
}
