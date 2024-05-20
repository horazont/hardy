use super::*;
use hardy_cbor as cbor;
use tokio::sync::mpsc::*;
use utils::{cancel::cancellable_sleep, settings};

const WAIT_SAMPLE_INTERVAL_SECS: u64 = 60;

#[derive(Clone)]
struct Config {
    admin_endpoints: bundle::AdminEndpoints,
    status_reports: bool,
    max_forwarding_delay: u32,
}

impl Config {
    fn new(config: &config::Config, admin_endpoints: bundle::AdminEndpoints) -> Self {
        let config = Self {
            admin_endpoints,
            status_reports: settings::get_with_default(config, "status_reports", false)
                .trace_expect("Invalid 'status_reports' value in configuration"),
            max_forwarding_delay: settings::get_with_default(config, "max_forwarding_delay", 5u32)
                .trace_expect("Invalid 'max_forwarding_delay' value in configuration"),
        };

        if !config.status_reports {
            info!("Bundle status reports are disabled by configuration");
        }

        if config.max_forwarding_delay == 0 {
            info!("Forwarding synchronization delay disabled by configuration");
        }

        config
    }
}

#[derive(Clone)]
pub struct Dispatcher {
    config: Config,
    store: store::Store,
    tx: Sender<(bundle::Metadata, bundle::Bundle)>,
    cla_registry: cla_registry::ClaRegistry,
    app_registry: app_registry::AppRegistry,
    fib: Option<fib::Fib>,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &config::Config,
        admin_endpoints: bundle::AdminEndpoints,
        store: store::Store,
        cla_registry: cla_registry::ClaRegistry,
        app_registry: app_registry::AppRegistry,
        fib: Option<fib::Fib>,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Self {
        // Load config
        let config = Config::new(config, admin_endpoints);

        // Create a channel for bundles
        let (tx, rx) = channel(16);
        let dispatcher = Self {
            config,
            store,
            tx,
            cla_registry,
            app_registry,
            fib,
        };

        // Spawn a bundle receiver
        let dispatcher_cloned = dispatcher.clone();
        let cancel_token_cloned = cancel_token.clone();
        task_set.spawn(async move {
            Self::pipeline_pump(dispatcher_cloned, rx, cancel_token_cloned).await
        });

        // Spawn a waiter
        let dispatcher_cloned = dispatcher.clone();
        task_set.spawn(async move { Self::check_waiting(dispatcher_cloned, cancel_token).await });

        dispatcher
    }

    async fn enqueue_bundle(
        &self,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
    ) -> Result<(), Error> {
        // Put bundle into channel
        self.tx.send((metadata, bundle)).await.map_err(|e| e.into())
    }

    #[instrument(skip_all)]
    async fn pipeline_pump(
        self,
        mut rx: Receiver<(bundle::Metadata, bundle::Bundle)>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                bundle = rx.recv() => match bundle {
                    None => break,
                    Some((metadata,bundle)) => {
                        let dispatcher = self.clone();
                        let cancel_token_cloned = cancel_token.clone();
                        task_set.spawn(async move {
                            dispatcher.process_bundle(metadata,bundle,cancel_token_cloned).await.trace_expect("Failed to process bundle");
                        });
                    }
                },
                Some(r) = task_set.join_next() => r.trace_expect("Task terminated unexpectedly"),
                _ = cancel_token.cancelled() => break
            }
        }

        // Wait for all sub-tasks to complete
        while let Some(r) = task_set.join_next().await {
            r.trace_expect("Task terminated unexpectedly")
        }
    }

    #[instrument(skip_all)]
    async fn check_waiting(self, cancel_token: tokio_util::sync::CancellationToken) {
        let timer = tokio::time::sleep(tokio::time::Duration::from_secs(WAIT_SAMPLE_INTERVAL_SECS));
        tokio::pin!(timer);

        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                () = &mut timer => {
                    // Determine next interval before we do any other waiting
                    let interval = tokio::time::Instant::now() + tokio::time::Duration::from_secs(WAIT_SAMPLE_INTERVAL_SECS);

                    // Get all bundles that are ready before now() + WAIT_SAMPLE_INTERVAL_SECS
                    let waiting = self.store.get_waiting_bundles(time::OffsetDateTime::now_utc() + time::Duration::new(WAIT_SAMPLE_INTERVAL_SECS as i64, 0)).await.trace_expect("get_waiting_bundles failed");
                    for (metadata,bundle,until) in waiting {
                        // Spawn a task for each ready bundle
                        let dispatcher = self.clone();
                        let cancel_token_cloned = cancel_token.clone();
                        task_set.spawn(async move {
                            dispatcher.delay_bundle(metadata,bundle, until,cancel_token_cloned).await.trace_expect("Failed to process bundle");
                        });
                    }

                    timer.as_mut().reset(interval);
                },
                Some(r) = task_set.join_next() => r.trace_expect("Task terminated unexpectedly"),
                _ = cancel_token.cancelled() => break
            }
        }

        // Wait for all sub-tasks to complete
        while let Some(r) = task_set.join_next().await {
            r.trace_expect("Task terminated unexpectedly")
        }
    }

    async fn delay_bundle(
        &self,
        mut metadata: bundle::Metadata,
        bundle: bundle::Bundle,
        until: time::OffsetDateTime,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), Error> {
        // Check if it's worth us waiting (safety check for metadata storage clock drift)
        let wait = until - time::OffsetDateTime::now_utc();
        if wait > time::Duration::new(WAIT_SAMPLE_INTERVAL_SECS as i64, 0) {
            // Nothing to do now, it will be picked up later
            trace!("Bundle will wait offline until: {until}");
            return Ok(());
        }

        if let bundle::BundleStatus::ForwardAckPending(_, _) = &metadata.status {
            trace!("Waiting for bundle forwarding acknowledgement inline until: {until}");
        } else {
            trace!("Waiting to forward bundle inline until: {until}");
        }

        // Wait a bit
        if !cancellable_sleep(wait, &cancel_token).await {
            // Cancelled
            return Ok(());
        }

        if let bundle::BundleStatus::ForwardAckPending(_, _) = &metadata.status {
            // Check if the bundle has been acknowledged while we slept
            let Some(bundle::BundleStatus::ForwardAckPending(_, _)) =
                self.store.check_status(&metadata.storage_name).await?
            else {
                // It's not longer waiting, our work here is done
                return Ok(());
            };
        }

        trace!("Forwarding bundle");

        // Set status to ForwardPending
        metadata.status = bundle::BundleStatus::ForwardPending;
        self.store
            .set_status(&metadata.storage_name, &metadata.status)
            .await?;

        // And forward it!
        self.forward_bundle(metadata, bundle, cancel_token).await
    }

    #[instrument(skip(self))]
    pub async fn process_bundle(
        &self,
        mut metadata: bundle::Metadata,
        mut bundle: bundle::Bundle,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), Error> {
        if let bundle::BundleStatus::DispatchPending = &metadata.status {
            // Check if we are the final destination
            metadata.status = if self
                .config
                .admin_endpoints
                .is_local_service(&bundle.destination)
            {
                if bundle.id.fragment_info.is_some() {
                    // Reassembly!!
                    trace!("Bundle requires fragment reassembly");
                    bundle::BundleStatus::ReassemblyPending
                } else {
                    // The bundle is ready for collection
                    trace!("Bundle is ready for local delivery");
                    bundle::BundleStatus::CollectionPending
                }
            } else {
                // Forward to another BPA
                trace!("Forwarding bundle");
                bundle::BundleStatus::ForwardPending
            };

            self.store
                .set_status(&metadata.storage_name, &metadata.status)
                .await?;
        }

        if let bundle::BundleStatus::ReassemblyPending = &metadata.status {
            // Attempt reassembly
            let Some((m, b)) = self.reassemble(metadata, bundle).await? else {
                // Waiting for more fragments to arrive
                return Ok(());
            };
            (metadata, bundle) = (m, b);
        }

        match &metadata.status {
            bundle::BundleStatus::IngressPending
            | bundle::BundleStatus::DispatchPending
            | bundle::BundleStatus::ReassemblyPending => {
                unreachable!()
            }
            bundle::BundleStatus::CollectionPending => {
                // Check if we have a local service registered
                if let Some(endpoint) = self.app_registry.find_by_eid(&bundle.destination) {
                    // Notify that the bundle is ready for collection
                    trace!("Notifying application that bundle is ready for collection");
                    endpoint.collection_notify(&bundle.id).await;
                }
                Ok(())
            }
            bundle::BundleStatus::ForwardAckPending(_, _) => {
                // Clear the pending ACK, we are reprocessing
                metadata.status = bundle::BundleStatus::ForwardPending;
                self.store
                    .set_status(&metadata.storage_name, &metadata.status)
                    .await?;

                // And just forward
                self.forward_bundle(metadata, bundle, cancel_token).await
            }
            bundle::BundleStatus::ForwardPending => {
                self.forward_bundle(metadata, bundle, cancel_token).await
            }
            bundle::BundleStatus::Waiting(until) => {
                let until = *until;
                self.delay_bundle(metadata, bundle, until, cancel_token)
                    .await
            }
            bundle::BundleStatus::Tombstone => Ok(()),
        }
    }

    async fn forward_bundle(
        &self,
        mut metadata: bundle::Metadata,
        bundle: bundle::Bundle,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), Error> {
        // Check bundle expiry
        if bundle::has_bundle_expired(&metadata, &bundle) {
            trace!("Bundle lifetime has expired");
            return self
                .drop_bundle(
                    metadata,
                    bundle,
                    Some(bundle::StatusReportReasonCode::LifetimeExpired),
                )
                .await;
        }

        let Some(fib) = &self.fib else {
            /* If forwarding is disabled in the configuration, then we can only deliver bundles.
             * As we have decided that the bundle is not for a local service, we cannot deliver.
             * Therefore, we respond with a Destination endpoint ID unavailable report */
            trace!("Bundle should be forwarded, but forwarding is disabled");
            return self
                .drop_bundle(
                    metadata,
                    bundle,
                    Some(bundle::StatusReportReasonCode::DestinationEndpointIDUnavailable),
                )
                .await;
        };

        // Resolve destination
        let Ok(mut destination) = bundle.destination.clone().try_into() else {
            // Bundle destination is not a valid next-hop
            trace!(
                "Bundle has invalid destination for forwarding: {}",
                bundle.destination
            );
            return self
                .drop_bundle(
                    metadata,
                    bundle,
                    Some(bundle::StatusReportReasonCode::DestinationEndpointIDUnavailable),
                )
                .await;
        };

        // TODO: Pluggable Egress filters!

        /* We loop here, as the FIB could tell us that there should be a CLA to use to forward
         * But it might be rebooting or jammed, so we keep retrying for a "reasonable" amount of time */
        let mut data = None;
        let mut data_is_time_sensitive = false;
        let mut previous = false;
        let mut retries = 0;
        let mut congestion_wait = None;
        let mut actions = fib.find(&destination).into_iter();
        let reason = loop {
            // Lookup/Perform actions
            match actions.next() {
                Some(fib::ForwardAction::Drop(reason)) => {
                    trace!("Bundle is black-holed");
                    break reason;
                }
                Some(fib::ForwardAction::Wait(until)) => {
                    // Check to see if waiting is even worth it
                    if until > bundle::get_bundle_expiry(&metadata, &bundle) {
                        trace!("Bundle lifetime is shorter than wait period");
                        break Some(
                            bundle::StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute,
                        );
                    }

                    // Wait a bit
                    if !self
                        .wait_to_forward(&metadata, until, &cancel_token)
                        .await?
                    {
                        // Cancelled, or too long a wait for here
                        return Ok(());
                    }

                    // Restart lookup
                    retries = 0;
                    actions = fib.find(&destination).into_iter();
                }
                Some(fib::ForwardAction::Forward(a)) => {
                    // Find the named CLA
                    if let Some(endpoint) = self.cla_registry.find_by_name(&a.name) {
                        // Get bundle data from store, now we know we need it!
                        if data.is_none() {
                            (data, data_is_time_sensitive) = match self
                                .store
                                .load_data(&metadata.storage_name)
                                .await
                            {
                                Ok(data) => self
                                    .update_extension_blocks(&metadata, &bundle, (*data).as_ref())
                                    .map(|(data, data_time_sensitive)| {
                                        (Some(data), data_time_sensitive)
                                    })?,
                                Err(e) => {
                                    // The bundle data has gone!
                                    warn!("Failed to load bundle data: {e}");
                                    break Some(bundle::StatusReportReasonCode::DepletedStorage);
                                }
                            };
                        }

                        match endpoint
                            .forward_bundle(a.address.clone(), data.clone().unwrap())
                            .await
                        {
                            Ok((None, None)) => {
                                // We have successfully forwarded!
                                self.report_bundle_forwarded(&metadata, &bundle).await?;
                                break None;
                            }
                            Ok((Some(token), until)) => {
                                // CLA will report successful forwarding
                                // Don't wait longer than expiry
                                let until = until.unwrap_or_else(|| {
                                    warn!("CLA endpoint has not provided a suitable AckPending delay, defaulting to 1 minute");
                                    time::OffsetDateTime::now_utc() + time::Duration::minutes(1)
                                }).min(bundle::get_bundle_expiry(&metadata, &bundle));

                                // Set the bundle status to 'Forward Acknowledgement Pending'
                                metadata.status =
                                    bundle::BundleStatus::ForwardAckPending(token, until);
                                return self
                                    .store
                                    .set_status(&metadata.storage_name, &metadata.status)
                                    .await;
                            }
                            Ok((None, until)) => {
                                let until = until.unwrap_or_else(|| {
                                    warn!("CLA endpoint has not provided a suitable Congestion delay, defaulting to 10 seconds");
                                    time::OffsetDateTime::now_utc() + time::Duration::seconds(10)
                                });
                                trace!("CLA reported congestion, retry at: {until}");

                                // Remember the shortest wait for a retry, in case we have ECMP
                                congestion_wait = congestion_wait
                                    .map_or(Some(until), |w: time::OffsetDateTime| {
                                        Some(w.min(until))
                                    })
                            }
                            Err(e) => trace!("CLA failed to forward {e}"),
                        }
                    } else {
                        trace!("FIB has entry for unknown endpoint: {}", a.name);
                    }
                    // Try the next CLA, this one is busy, broken or missing
                }
                None => {
                    // Check for congestion
                    if let Some(until) = congestion_wait {
                        trace!("All available CLAs report congestion until {until}");

                        // Check to see if waiting is even worth it
                        if until > bundle::get_bundle_expiry(&metadata, &bundle) {
                            trace!("Bundle lifetime is shorter than wait period");
                            break Some(
                                bundle::StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute,
                            );
                        }

                        // We must wait for a bit for the CLAs to calm down
                        if !self
                            .wait_to_forward(&metadata, until, &cancel_token)
                            .await?
                        {
                            // Cancelled, or too long a wait for here
                            return Ok(());
                        }

                        // Reset retry counter, as we found a route, it's just busy
                        congestion_wait = None;
                        retries = 0;
                    } else if retries > self.config.max_forwarding_delay {
                        if previous {
                            // We have delayed long enough trying to find a route to previous_node
                            trace!("Timed out trying to forward bundle to previous node");
                            break Some(
                                bundle::StatusReportReasonCode::NoKnownRouteToDestinationFromHere,
                            );
                        }

                        trace!("Timed out trying to forward bundle");

                        // Return the bundle to the source via the 'previous_node' or 'bundle.source'
                        if let Ok(previous_node) = bundle
                            .previous_node
                            .clone()
                            .unwrap_or(bundle.id.source.clone())
                            .try_into()
                        {
                            // Try the previous_node
                            destination = previous_node;
                        } else {
                            // Previous node is not a valid next-hop
                            trace!("Bundle has no valid previous node to return to");
                            break Some(
                                bundle::StatusReportReasonCode::DestinationEndpointIDUnavailable,
                            );
                        }

                        // Reset retry counter as we are attempting to return the bundle
                        trace!("Returning bundle to previous node: {destination}");
                        previous = true;
                        retries = 0;
                    } else {
                        retries = retries.saturating_add(1);

                        trace!("Retrying ({retries}) FIB lookup to allow FIB and CLAs to resync");

                        // Async sleep for 1 second
                        if !cancellable_sleep(time::Duration::seconds(1), &cancel_token).await {
                            // Cancelled
                            return Ok(());
                        }
                    }

                    // Check bundle expiry
                    if bundle::has_bundle_expired(&metadata, &bundle) {
                        trace!("Bundle lifetime has expired");
                        break Some(bundle::StatusReportReasonCode::LifetimeExpired);
                    }

                    if data_is_time_sensitive {
                        // Force a reload of current data, because Bundle Age may have changed
                        data = None;
                    }

                    // Lookup again
                    actions = fib.find(&destination).into_iter();
                }
            }
        };

        self.drop_bundle(metadata, bundle, reason).await
    }

    async fn wait_to_forward(
        &self,
        metadata: &bundle::Metadata,
        until: time::OffsetDateTime,
        cancel_token: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, Error> {
        let wait = until - time::OffsetDateTime::now_utc();
        if wait > time::Duration::new(WAIT_SAMPLE_INTERVAL_SECS as i64, 0) {
            // Nothing to do now, set bundle status to Waiting, and it will be picked up later
            trace!("Bundle will wait offline until: {until}");
            self.store
                .set_status(
                    &metadata.storage_name,
                    &bundle::BundleStatus::Waiting(until),
                )
                .await?;
            return Ok(false);
        }

        // We must wait here, as we have missed the scheduled wait interval
        trace!("Waiting to forward bundle inline until: {until}");
        Ok(cancellable_sleep(wait, cancel_token).await)
    }

    fn update_extension_blocks(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
        data: &[u8],
    ) -> Result<(Vec<u8>, bool), Error> {
        let mut editor = bundle::Editor::new(bundle)
            // Previous Node Block
            .replace_extension_block(bundle::BlockType::PreviousNode)
            .must_replicate(true)
            .data(cbor::encode::emit_array(Some(1), |a| {
                a.emit(
                    &self
                        .config
                        .admin_endpoints
                        .get_admin_endpoint(&bundle.destination),
                )
            }))
            .build();

        // Increment Hop Count
        if let Some(mut hop_count) = bundle.hop_count {
            editor = editor
                .replace_extension_block(bundle::BlockType::HopCount)
                .must_replicate(true)
                .data(cbor::encode::emit_array(Some(2), |a| {
                    hop_count.count += 1;
                    a.emit(hop_count.limit);
                    a.emit(hop_count.count);
                }))
                .build();
        }

        // Update Bundle Age, if required
        let mut is_time_sensitive = false;
        if let Some(bundle_age) = bundle
            .age
            .map_or_else(
                || {
                    (bundle.id.timestamp.creation_time == 0)
                        .then(|| bundle::get_bundle_creation(metadata, bundle))
                },
                |_| Some(bundle::get_bundle_creation(metadata, bundle)),
            )
            .map(|creation| {
                (time::OffsetDateTime::now_utc() - creation)
                    .whole_milliseconds()
                    .clamp(0, u64::MAX as i128) as u64
            })
        {
            editor = editor
                .replace_extension_block(bundle::BlockType::BundleAge)
                .must_replicate(true)
                .data(cbor::encode::emit_array(Some(1), |a| a.emit(bundle_age)))
                .build();

            // If we have a bundle age, then we are time sensitive
            is_time_sensitive = true;
        }

        editor
            .build(data)
            .map(|(_, data)| data)
            .map(|data| (data, is_time_sensitive))
    }

    #[instrument(skip(self))]
    async fn drop_bundle(
        &self,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
        reason: Option<bundle::StatusReportReasonCode>,
    ) -> Result<(), Error> {
        if let Some(reason) = reason {
            self.report_bundle_deleted(&metadata, &bundle, reason)
                .await?;
        }
        trace!("Discarding bundle, leaving tombstone");
        self.store.remove(&metadata.storage_name).await
    }

    #[instrument(skip(self))]
    pub async fn report_bundle_reception(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
        reason: bundle::StatusReportReasonCode,
    ) -> Result<(), Error> {
        // Check if a report is requested
        if !self.config.status_reports || !bundle.flags.receipt_report_requested {
            return Ok(());
        }

        trace!("Reporting bundle reception to {}", &bundle.report_to);

        self.dispatch_status_report(
            new_bundle_status_report(metadata, bundle, reason, None, None, None),
            &bundle.report_to,
        )
        .await
    }

    #[instrument(skip(self))]
    pub async fn report_bundle_forwarded(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
    ) -> Result<(), Error> {
        // Check if a report is requested
        if !self.config.status_reports || !bundle.flags.forward_report_requested {
            return Ok(());
        }

        trace!("Reporting bundle forwarded to {}", &bundle.report_to);

        self.dispatch_status_report(
            new_bundle_status_report(
                metadata,
                bundle,
                bundle::StatusReportReasonCode::NoAdditionalInformation,
                Some(time::OffsetDateTime::now_utc()),
                None,
                None,
            ),
            &bundle.report_to,
        )
        .await
    }

    #[instrument(skip(self))]
    async fn report_bundle_delivered(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
    ) -> Result<(), Error> {
        // Check if a report is requested
        if !self.config.status_reports || !bundle.flags.forward_report_requested {
            return Ok(());
        }

        trace!("Reporting bundle delivered to {}", &bundle.report_to);

        // Create a bundle report
        self.dispatch_status_report(
            new_bundle_status_report(
                metadata,
                bundle,
                bundle::StatusReportReasonCode::NoAdditionalInformation,
                None,
                Some(time::OffsetDateTime::now_utc()),
                None,
            ),
            &bundle.report_to,
        )
        .await
    }

    #[instrument(skip(self))]
    pub async fn report_bundle_deleted(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
        reason: bundle::StatusReportReasonCode,
    ) -> Result<(), Error> {
        // Check if a report is requested
        if !self.config.status_reports || !bundle.flags.delete_report_requested {
            return Ok(());
        }

        trace!("Reporting bundle deleted to {}", &bundle.report_to);

        // Create a bundle report
        self.dispatch_status_report(
            new_bundle_status_report(
                metadata,
                bundle,
                reason,
                None,
                None,
                Some(time::OffsetDateTime::now_utc()),
            ),
            &bundle.report_to,
        )
        .await
    }

    async fn dispatch_status_report(
        &self,
        payload: Vec<u8>,
        report_to: &bundle::Eid,
    ) -> Result<(), Error> {
        // Create a bundle report
        let (bundle, data) = bundle::Builder::new()
            .is_admin_record(true)
            .source(&self.config.admin_endpoints.get_admin_endpoint(report_to))
            .destination(report_to)
            .add_payload_block(payload)
            .build()?;

        // Store to store
        let metadata = self
            .store
            .store(&bundle, data, bundle::BundleStatus::DispatchPending, None)
            .await?
            .trace_expect("Duplicate bundle created by builder!");

        // And queue it up
        self.enqueue_bundle(metadata, bundle).await
    }

    #[instrument(skip(self))]
    pub async fn local_dispatch(
        &self,
        source: bundle::Eid,
        destination: bundle::Eid,
        data: Vec<u8>,
        lifetime: Option<u64>,
        app_ack_requested: bool,
        do_not_fragment: bool,
    ) -> Result<(), Error> {
        // Build the bundle
        let mut b = bundle::Builder::new()
            .source(&source)
            .destination(&destination);

        // Set flags
        if app_ack_requested || do_not_fragment {
            b = b
                .app_ack_requested(app_ack_requested)
                .do_not_fragment(do_not_fragment)
                .report_to(&self.config.admin_endpoints.get_admin_endpoint(&destination));
        }

        // Lifetime
        if let Some(lifetime) = lifetime {
            b = b.lifetime(lifetime);
        }

        // Add payload and build
        let (bundle, data) = b.add_payload_block(data).build()?;

        // Store to store
        let metadata = self
            .store
            .store(&bundle, data, bundle::BundleStatus::DispatchPending, None)
            .await?
            .trace_expect("Duplicate bundle created by builder!");

        // And queue it up
        self.enqueue_bundle(metadata, bundle).await
    }

    async fn reassemble(
        &self,
        _metadata: bundle::Metadata,
        _bundle: bundle::Bundle,
    ) -> Result<Option<(bundle::Metadata, bundle::Bundle)>, Error> {
        todo!()
    }

    #[instrument(skip(self))]
    pub async fn collect(
        &self,
        destination: bundle::Eid,
        bundle_id: String,
    ) -> Result<Option<(String, Vec<u8>, time::OffsetDateTime)>, Error> {
        // Lookup bundle
        let Some((metadata, bundle)) = self
            .store
            .load(&bundle::BundleId::from_key(&bundle_id)?)
            .await?
        else {
            return Ok(None);
        };

        // Double check that we are returning something valid
        if let bundle::BundleStatus::CollectionPending = &metadata.status {
            let expiry = bundle::get_bundle_expiry(&metadata, &bundle);
            if expiry <= time::OffsetDateTime::now_utc() || bundle.destination != destination {
                return Ok(None);
            }

            // Get the data!
            let data = self.store.load_data(&metadata.storage_name).await?;

            // By the time we get here, we're safe to report delivery
            self.report_bundle_delivered(&metadata, &bundle).await?;

            Ok(Some((
                bundle.id.to_key(),
                (*data).as_ref().to_vec(),
                expiry,
            )))
        } else {
            Ok(None)
        }
    }

    #[instrument(skip(self))]
    pub async fn poll_for_collection(
        &self,
        destination: bundle::Eid,
    ) -> Result<Vec<(String, time::OffsetDateTime)>, Error> {
        self.store.poll_for_collection(destination).await
    }
}

fn new_bundle_status_report(
    metadata: &bundle::Metadata,
    bundle: &bundle::Bundle,
    reason: bundle::StatusReportReasonCode,
    forwarded: Option<time::OffsetDateTime>,
    delivered: Option<time::OffsetDateTime>,
    deleted: Option<time::OffsetDateTime>,
) -> Vec<u8> {
    cbor::encode::emit_array(Some(2), |a| {
        a.emit(1);
        a.emit_array(Some(bundle.id.fragment_info.map_or(4, |_| 6)), |a| {
            // Statuses
            a.emit_array(Some(4), |a| {
                // Report node received bundle
                match metadata.received_at {
                    Some(received_at)
                        if bundle.flags.report_status_time
                            && bundle.flags.receipt_report_requested =>
                    {
                        a.emit_array(Some(2), |a| {
                            a.emit(true);
                            a.emit(bundle::as_dtn_time(&received_at))
                        })
                    }
                    _ => a.emit_array(Some(1), |a| a.emit(bundle.flags.receipt_report_requested)),
                }

                // Report node forwarded the bundle
                match forwarded {
                    Some(forwarded)
                        if bundle.flags.report_status_time
                            && bundle.flags.forward_report_requested =>
                    {
                        a.emit_array(Some(2), |a| {
                            a.emit(true);
                            a.emit(bundle::as_dtn_time(&forwarded))
                        })
                    }
                    Some(_) => {
                        a.emit_array(Some(1), |a| a.emit(bundle.flags.forward_report_requested))
                    }
                    _ => a.emit_array(Some(1), |a| a.emit(false)),
                }

                // Report node delivered the bundle
                match delivered {
                    Some(delivered)
                        if bundle.flags.report_status_time
                            && bundle.flags.delivery_report_requested =>
                    {
                        a.emit_array(Some(2), |a| {
                            a.emit(true);
                            a.emit(bundle::as_dtn_time(&delivered))
                        })
                    }
                    Some(_) => {
                        a.emit_array(Some(1), |a| a.emit(bundle.flags.delivery_report_requested))
                    }
                    _ => a.emit_array(Some(1), |a| a.emit(false)),
                }

                // Report node deleted the bundle
                match deleted {
                    Some(deleted)
                        if bundle.flags.report_status_time
                            && bundle.flags.delete_report_requested =>
                    {
                        a.emit_array(Some(2), |a| {
                            a.emit(true);
                            a.emit(bundle::as_dtn_time(&deleted))
                        })
                    }
                    Some(_) => {
                        a.emit_array(Some(1), |a| a.emit(bundle.flags.delete_report_requested))
                    }
                    _ => a.emit_array(Some(1), |a| a.emit(false)),
                }
            });

            // Reason code
            a.emit(reason);
            // Source EID
            a.emit(&bundle.id.source);
            // Creation Timestamp
            a.emit(&bundle.id.timestamp);

            if let Some(fragment_info) = &bundle.id.fragment_info {
                // Add fragment info
                a.emit(fragment_info.offset);
                a.emit(fragment_info.total_len);
            }
        })
    })
}
