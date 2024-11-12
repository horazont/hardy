use super::*;

#[derive(Default, Debug)]
pub struct SendRequest {
    pub source: bpv7::Eid,
    pub destination: bpv7::Eid,
    pub data: Bytes,
    pub lifetime: Option<u64>,
    pub flags: Option<bpv7::BundleFlags>,
}

impl Dispatcher {
    #[instrument(skip(self))]
    pub async fn local_dispatch(&self, mut request: SendRequest) -> Result<(), Error> {
        // Check to see if we should use ipn 2-element encoding
        if let bpv7::Eid::Ipn {
            allocator_id: da,
            node_number: dn,
            service_number: ds,
        } = &request.destination
        {
            // Check configured entries
            if !self
                .config
                .ipn_2_element
                .find(&request.destination)
                .is_empty()
            {
                if let bpv7::Eid::Ipn {
                    allocator_id: sa,
                    node_number: sn,
                    service_number: ss,
                } = &request.source
                {
                    request.source = bpv7::Eid::LegacyIpn {
                        allocator_id: *sa,
                        node_number: *sn,
                        service_number: *ss,
                    };
                }
                request.destination = bpv7::Eid::LegacyIpn {
                    allocator_id: *da,
                    node_number: *dn,
                    service_number: *ds,
                };
            }
        }

        // Build the bundle
        let mut b = bpv7::Builder::new();

        // Set flags
        if let Some(flags) = request.flags {
            b = b.flags(flags).report_to(
                self.config
                    .admin_endpoints
                    .get_admin_endpoint(&request.destination),
            );
        }

        // Lifetime
        if let Some(lifetime) = request.lifetime {
            b = b.lifetime(lifetime);
        }

        // Build the bundle
        let (bundle, data) = b
            .source(request.source)
            .destination(request.destination)
            .add_payload_block(request.data.into())
            .build();

        // Store to store
        let metadata = self
            .store
            .store(&bundle, &data, metadata::BundleStatus::default(), None)
            .await?
            .trace_expect("Duplicate bundle generated by builder!");

        // And get it dispatched
        self.dispatch_bundle(metadata::Bundle { metadata, bundle })
            .await
    }
}
