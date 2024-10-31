use hardy_cbor as cbor;

mod block;
mod block_flags;
mod block_type;
mod bpsec;
mod builder;
mod bundle;
mod bundle_flags;
mod bundle_id;
mod crc;
mod creation_timestamp;
mod dtn_time;
mod editor;
mod eid;
mod eid_pattern;
mod eid_pattern_map;
mod hop_info;
mod primary_block;
mod status_report;

pub mod prelude {
    pub use super::block::Block;
    pub use super::block_flags::BlockFlags;
    pub use super::block_type::BlockType;
    pub use super::builder::Builder;
    pub use super::bundle::{Bundle, BundleError, ValidBundle};
    pub use super::bundle_flags::BundleFlags;
    pub use super::bundle_id::{BundleId, FragmentInfo};
    pub use super::crc::CrcType;
    pub use super::creation_timestamp::CreationTimestamp;
    pub use super::dtn_time::DtnTime;
    pub use super::editor::Editor;
    pub use super::eid::{Eid, EidError};
    pub use super::eid_pattern::{EidPattern, EidPatternError};
    pub use super::eid_pattern_map::EidPatternMap;
    pub use super::hop_info::HopInfo;
    pub use super::status_report::{
        AdministrativeRecord, BundleStatusReport, StatusAssertion, StatusReportError,
        StatusReportReasonCode,
    };
}

// Use prelude types internally
use prelude::*;
