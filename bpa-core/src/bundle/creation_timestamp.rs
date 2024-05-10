use super::*;

#[derive(Default, Debug, Copy, Clone)]
pub struct CreationTimestamp {
    pub creation_time: u64,
    pub sequence_number: u64,
}

impl cbor::encode::ToCbor for &CreationTimestamp {
    fn to_cbor(self, encoder: &mut cbor::encode::Encoder) {
        encoder.emit_array(Some(2), |a| {
            a.emit(self.creation_time);
            a.emit(self.sequence_number);
        })
    }
}

impl cbor::decode::FromCbor for CreationTimestamp {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        cbor::decode::parse_array(data, |a, tags| {
            let ct = CreationTimestamp {
                creation_time: a.parse()?,
                sequence_number: a.parse()?,
            };
            a.end_or_else(|| anyhow!("Additional items found in Creation timestamp array"))?;
            Ok((ct, tags.to_vec()))
        })
        .map(|((t, tags), len)| (t, len, tags))
    }
}
