use super::*;

impl Dispatcher {
    pub(super) async fn forward_bundle(
        &self,
        bundle: &mut metadata::Bundle,
    ) -> Result<DispatchResult, Error> {
        let Some(fib) = &self.fib else {
            /* If forwarding is disabled in the configuration, then we can only deliver bundles.
             * As we have decided that the bundle is not for a local service, we cannot deliver.
             * Therefore, we respond with a Destination endpoint ID unavailable report */
            trace!("Bundle should be forwarded, but forwarding is disabled");
            return Ok(DispatchResult::Drop(Some(
                bpv7::StatusReportReasonCode::DestinationEndpointIDUnavailable,
            )));
        };

        // TODO: Pluggable Egress filters!

        /* We loop here, as the FIB could tell us that there should be a CLA to use to forward
         * But it might be rebooting or jammed, so we keep retrying for a "reasonable" amount of time */
        let mut previous = false;
        let mut retries = 0;
        let mut destination = &bundle.bundle.destination;

        loop {
            // Check bundle expiry
            if bundle.has_expired() {
                trace!("Bundle lifetime has expired");
                return Ok(DispatchResult::Drop(Some(
                    bpv7::StatusReportReasonCode::LifetimeExpired,
                )));
            }

            // Lookup/Perform actions
            let action = match fib.find(destination).await {
                Err(reason) => {
                    trace!("Bundle is black-holed");
                    return Ok(DispatchResult::Drop(reason));
                }
                Ok(fib::ForwardAction {
                    clas,
                    until: Some(until),
                }) if clas.is_empty() => {
                    return self.bundle_wait(bundle, until).await;
                }
                Ok(action) => action,
            };

            let mut congestion_wait = None;

            // For each CLA
            for endpoint in &action.clas {
                // Find the named CLA
                if let Some(e) = self.cla_registry.find(endpoint.handle).await {
                    // Get bundle data from store, now we know we need it!

                    let Some(source_data) = self.load_data(bundle).await? else {
                        // Bundle data was deleted sometime during processing
                        return Ok(DispatchResult::Done);
                    };

                    // Increment Hop Count, etc...
                    let data = self.update_extension_blocks(bundle, (*source_data).as_ref())?;

                    match e.forward_bundle(destination, data.into()).await {
                        Ok(cla_registry::ForwardBundleResult::Sent) => {
                            // We have successfully forwarded!
                            return self
                                .report_bundle_forwarded(bundle)
                                .await
                                .map(|_| DispatchResult::Drop(None));
                        }
                        Ok(cla_registry::ForwardBundleResult::Pending(handle, until)) => {
                            // CLA will report successful forwarding
                            // Don't wait longer than expiry
                            let until = until.unwrap_or_else(|| {
                                warn!("CLA endpoint has not provided a suitable AckPending delay, defaulting to 1 minute");
                                time::OffsetDateTime::now_utc() + time::Duration::minutes(1)
                            }).min(bundle.expiry());

                            // Set the bundle status to 'Forward Acknowledgement Pending' and re-dispatch
                            return self
                                .store
                                .set_status(
                                    bundle,
                                    metadata::BundleStatus::ForwardAckPending(handle, until),
                                )
                                .await
                                .map(|_| DispatchResult::Continue);
                        }
                        Ok(cla_registry::ForwardBundleResult::Congested(until)) => {
                            trace!("CLA reported congestion, retry at: {until}");

                            // Remember the shortest wait for a retry, in case we have ECMP
                            congestion_wait = congestion_wait
                                .map_or(Some(until), |w: time::OffsetDateTime| Some(w.min(until)))
                        }
                        Err(e) => trace!("CLA failed to forward {e}"),
                    }
                } else {
                    trace!("FIB has entry for unknown CLA: {endpoint:?}");
                }
                // Try the next CLA, this one is busy, broken or missing
            }

            // By the time we get here, we have tried every CLA

            // Check for congestion
            if let Some(mut until) = congestion_wait {
                // We must wait for a bit for the CLAs to calm down
                trace!("All available CLAs report congestion until {until}");

                // Limit congestion wait to the forwarding wait
                if let Some(wait) = action.until {
                    until = wait.min(until);
                }

                return self.bundle_wait(bundle, until).await;
            } else if retries >= self.config.max_forwarding_delay {
                if previous {
                    // We have delayed long enough trying to find a route to previous_node
                    trace!("Failed to return bundle to previous node, no route");
                    return Ok(DispatchResult::Drop(Some(
                        bpv7::StatusReportReasonCode::NoKnownRouteToDestinationFromHere,
                    )));
                }

                trace!("Failed to forward bundle, no route");

                // Return the bundle to the source via the 'previous_node' or 'bundle.source'
                destination = bundle
                    .bundle
                    .previous_node
                    .as_ref()
                    .unwrap_or(&bundle.bundle.id.source);

                trace!("Returning bundle to previous node: {destination}");

                // Reset retry counter as we are attempting to return the bundle
                retries = 0;
                previous = true;
            } else {
                retries = retries.saturating_add(1);

                trace!("Retrying ({retries}) FIB lookup to allow FIB and CLAs to resync");

                // Async sleep for 1 second
                if !cancellable_sleep(time::Duration::seconds(1), &self.cancel_token).await {
                    // Cancelled
                    return Ok(DispatchResult::Done);
                }
            }
        }
    }

    fn update_extension_blocks(
        &self,
        bundle: &metadata::Bundle,
        data: &[u8],
    ) -> Result<Vec<u8>, Error> {
        let mut editor = bpv7::Editor::new(&bundle.bundle);

        // Remove unrecognized blocks we are supposed to
        for (block_number, block) in &bundle.bundle.blocks {
            if let bpv7::BlockType::Unrecognised(_) = &block.block_type {
                if block.flags.delete_block_on_failure {
                    editor = editor.remove_extension_block(*block_number);
                }
            }
        }

        // Previous Node Block
        editor = editor
            .replace_extension_block(bpv7::BlockType::PreviousNode)
            .data(cbor::encode::emit(
                &self
                    .config
                    .admin_endpoints
                    .get_admin_endpoint(&bundle.bundle.destination),
            ))
            .build();

        // Increment Hop Count
        if let Some(hop_count) = &bundle.bundle.hop_count {
            editor = editor
                .replace_extension_block(bpv7::BlockType::HopCount)
                .data(cbor::encode::emit(&bpv7::HopInfo {
                    limit: hop_count.limit,
                    count: hop_count.count + 1,
                }))
                .build();
        }

        // Update Bundle Age, if required
        if bundle.bundle.age.is_some() || bundle.bundle.id.timestamp.creation_time.is_none() {
            // We have a bundle age block already, or no valid clock at bundle source
            // So we must add an updated bundle age block
            let bundle_age = (time::OffsetDateTime::now_utc() - bundle.creation_time())
                .whole_milliseconds()
                .clamp(0, u64::MAX as i128) as u64;

            editor = editor
                .replace_extension_block(bpv7::BlockType::BundleAge)
                .data(cbor::encode::emit(bundle_age))
                .build();
        }

        editor.build(data).map_err(Into::into)
    }

    #[instrument(skip(self))]
    pub async fn confirm_forwarding(
        &self,
        handle: u32,
        bundle_id: &str,
    ) -> Result<(), tonic::Status> {
        let Some(bundle) = self
            .store
            .load(
                &bpv7::BundleId::from_key(bundle_id)
                    .map_err(|e| tonic::Status::from_error(e.into()))?,
            )
            .await
            .map_err(tonic::Status::from_error)?
        else {
            return Err(tonic::Status::not_found("No such bundle"));
        };

        match &bundle.metadata.status {
            metadata::BundleStatus::ForwardAckPending(t, _) if t == &handle => {
                // Report bundle forwarded
                self.report_bundle_forwarded(&bundle)
                    .await
                    .map_err(tonic::Status::from_error)?;

                // And drop the bundle
                self.drop_bundle(bundle, None)
                    .await
                    .map_err(tonic::Status::from_error)
            }
            _ => Err(tonic::Status::not_found("No such bundle")),
        }
    }
}
