use super::*;
use tokio::sync::mpsc::*;

pub struct ClaSource {
    pub protocol: String,
    pub address: Vec<u8>,
}

pub struct Ingress {
    store: store::Store,
    reassembler: reassembler::Reassembler,
    dispatcher: dispatcher::Dispatcher,
    receive_channel: Sender<(Option<ClaSource>, String, Option<time::OffsetDateTime>)>,
    ingress_channel: Sender<(bundle::Metadata, bundle::Bundle)>,
}

impl Clone for Ingress {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            dispatcher: self.dispatcher.clone(),
            reassembler: self.reassembler.clone(),
            receive_channel: self.receive_channel.clone(),
            ingress_channel: self.ingress_channel.clone(),
        }
    }
}

impl Ingress {
    pub fn new(
        _config: &config::Config,
        store: store::Store,
        reassembler: reassembler::Reassembler,
        dispatcher: dispatcher::Dispatcher,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        // Create a channel for new bundles
        let (receive_channel, receive_channel_rx) = channel(16);
        let (ingress_channel, ingress_channel_rx) = channel(16);
        let ingress = Self {
            store,
            reassembler,
            dispatcher,
            receive_channel,
            ingress_channel,
        };

        // Spawn a bundle receiver
        let ingress_cloned = ingress.clone();
        task_set.spawn(async move {
            Self::pipeline_pump(
                ingress_cloned,
                receive_channel_rx,
                ingress_channel_rx,
                cancel_token,
            )
            .await
        });

        Ok(ingress)
    }

    pub async fn receive(
        &self,
        from: Option<ClaSource>,
        data: Vec<u8>,
    ) -> Result<(), anyhow::Error> {
        // Capture received_at as soon as possible
        let received_at = time::OffsetDateTime::now_utc();

        // Write the bundle data to the store
        let storage_name = self.store.store_data(data).await?;

        // Put bundle into receive channel
        self.receive_channel
            .send((from, storage_name, Some(received_at)))
            .await
            .map_err(|e| e.into())
    }

    pub async fn enqueue_receive_bundle(
        &self,
        storage_name: &str,
        received_at: Option<time::OffsetDateTime>,
    ) -> Result<(), anyhow::Error> {
        // Put bundle into receive channel
        self.receive_channel
            .send((None, storage_name.to_string(), received_at))
            .await
            .map_err(|e| e.into())
    }

    pub async fn enqueue_ingress_bundle(
        &self,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        // Put bundle into ingress channel
        self.ingress_channel
            .send((metadata, bundle))
            .await
            .map_err(|e| e.into())
    }

    async fn pipeline_pump(
        self,
        mut receive_channel: Receiver<(Option<ClaSource>, String, Option<time::OffsetDateTime>)>,
        mut ingress_channel: Receiver<(bundle::Metadata, bundle::Bundle)>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                msg = receive_channel.recv() => match msg {
                    None => break,
                    Some((cla_source,storage_name,received_at)) => {
                        let ingress = self.clone();
                        task_set.spawn(async move {
                            ingress.process_receive_bundle(cla_source,storage_name,received_at).await.log_expect("Failed to process received bundle")
                        });
                    }
                },
                msg = ingress_channel.recv() => match msg {
                    None => break,
                    Some((metadata,bundle)) => {
                        let ingress = self.clone();
                        task_set.spawn(async move {
                            ingress.process_ingress_bundle(metadata,bundle).await.log_expect("Failed to process ingress bundle")
                        });
                    }
                },
                _ = cancel_token.cancelled() => break
            }
        }

        // Wait for all sub-tasks to complete
        while let Some(r) = task_set.join_next().await {
            r.log_expect("Task terminated unexpectedly")
        }
    }

    async fn process_receive_bundle(
        &self,
        from: Option<ClaSource>,
        storage_name: String,
        received_at: Option<time::OffsetDateTime>,
    ) -> Result<(), anyhow::Error> {
        // Parse the bundle
        let (metadata, bundle, valid) = {
            let data = self.store.load_data(&storage_name).await?;
            match bundle::parse((*data).as_ref()) {
                Ok((bundle, valid)) => (
                    bundle::Metadata {
                        status: bundle::BundleStatus::IngressPending,
                        storage_name,
                        hash: self.store.hash((*data).as_ref()),
                        received_at,
                    },
                    bundle,
                    valid,
                ),
                Err(e) => {
                    // Parse failed badly, no idea who to report to
                    log::info!("Bundle parsing failed: {}", e);
                    return Ok(());
                }
            }
        };

        // Report we have received the bundle
        self.dispatcher
            .report_bundle_reception(
                &metadata,
                &bundle,
                bundle::StatusReportReasonCode::NoAdditionalInformation,
            )
            .await?;

        /* RACE: If there is a crash between the report creation(above) and the metadata store (below)
         *  then we may send more than one "Received" Status Report when restarting,
         *  but that is currently considered benign (as a duplicate report causes little harm)
         *  and unlikely (as the report forwarding process is expected to take longer than the metadata.store)
         */

        // Store the bundle metadata in the store
        self.store.store_metadata(&metadata, &bundle).await?;

        if !valid {
            // Not valid, drop it
            self.dispatcher
                .report_bundle_deletion(
                    &metadata,
                    &bundle,
                    bundle::StatusReportReasonCode::BlockUnintelligible,
                )
                .await?;

            // Drop the bundle
            return self.store.remove(&metadata.storage_name).await;
        }

        if let Some(_from) = from {
            // TODO: Try to learn a route from `from`
        }

        // Process the bundle further
        self.process_ingress_bundle(metadata, bundle).await
    }

    async fn process_ingress_bundle(
        &self,
        mut metadata: bundle::Metadata,
        mut bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        if let bundle::BundleStatus::IngressPending = &metadata.status {
            // Check bundle blocks
            let reason;
            (reason, metadata, bundle) = self.check_extension_blocks(metadata, bundle).await?;

            if reason.is_none() {
                // TODO: Eid checks!
            }

            if reason.is_none() {
                // TODO: BPSec here!
            }

            if reason.is_none() {
                // TODO: Pluggable Ingress filters!
            }

            if let Some(reason) = reason {
                // Not valid, drop it
                self.dispatcher
                    .report_bundle_deletion(&metadata, &bundle, reason)
                    .await?;

                // Drop the bundle
                return self.store.remove(&metadata.storage_name).await;
            }

            metadata.status = if bundle.id.fragment_info.is_some() {
                // Fragments require reassembly
                bundle::BundleStatus::ReassemblyPending
            } else {
                // Dispatch!
                bundle::BundleStatus::DispatchPending
            };

            // Update the status
            self.store
                .set_status(&metadata.storage_name, metadata.status)
                .await?;
        }

        if let bundle::BundleStatus::ReassemblyPending = &metadata.status {
            // Send on to the reassembler
            self.reassembler.enqueue_bundle(metadata, bundle).await
        } else {
            // Just send it on to the dispatcher to deal with
            self.dispatcher.enqueue_bundle(metadata, bundle).await
        }
    }

    async fn check_extension_blocks(
        &self,
        mut metadata: bundle::Metadata,
        mut bundle: bundle::Bundle,
    ) -> Result<
        (
            Option<bundle::StatusReportReasonCode>,
            bundle::Metadata,
            bundle::Bundle,
        ),
        anyhow::Error,
    > {
        // Check for unsupported block types
        let mut blocks_to_remove = Vec::new();

        for (block_number, block) in &bundle.blocks {
            if let bundle::BlockType::Private(_) = &block.block_type {
                if block.flags.report_on_failure {
                    self.dispatcher
                        .report_bundle_reception(
                            &metadata,
                            &bundle,
                            bundle::StatusReportReasonCode::BlockUnsupported,
                        )
                        .await?;
                }

                if block.flags.delete_bundle_on_failure {
                    return Ok((
                        Some(bundle::StatusReportReasonCode::BlockUnsupported),
                        metadata,
                        bundle,
                    ));
                }

                if block.flags.delete_block_on_failure {
                    blocks_to_remove.push(*block_number);
                }
            }
        }

        // Rewrite bundle if needed
        if !blocks_to_remove.is_empty() {
            let mut r = bundle::Editor::new(metadata, bundle);
            for block_number in blocks_to_remove {
                r = r.remove_extension_block(block_number);
            }
            (metadata, bundle) = r.build(&self.store).await?;
        }
        Ok((None, metadata, bundle))
    }
}
