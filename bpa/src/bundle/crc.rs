use super::*;

const X25: ::crc::Crc<u16> = ::crc::Crc::<u16>::new(&::crc::CRC_16_IBM_SDLC);
const CASTAGNOLI: ::crc::Crc<u32> = ::crc::Crc::<u32>::new(&::crc::CRC_32_ISCSI);

pub fn parse_crc_value(
    data: &[u8],
    block_start: usize,
    block: &mut cbor::decode::Array,
    crc_type: CrcType,
) -> Result<(), anyhow::Error> {
    // Parse CRC
    let crc_value = block.try_parse_value(|value, crc_start, tags| match value {
        cbor::decode::Value::Bytes(crc, _) => {
            if !tags.is_empty() {
                log::info!("Parsing bundle block CRC value with tags");
            }
            match crc_type {
                CrcType::None => Err(anyhow!("Block has unexpected CRC value")),
                CrcType::CRC16_X25 => {
                    if crc.len() != 2 {
                        Err(anyhow!("Block has unexpected CRC value length"))
                    } else {
                        Ok((u16::from_be_bytes(crc.try_into()?) as u32, crc_start))
                    }
                }
                CrcType::CRC32_CASTAGNOLI => {
                    if crc.len() != 4 {
                        Err(anyhow!("Block has unexpected CRC value length"))
                    } else {
                        Ok((u32::from_be_bytes(crc.try_into()?), crc_start))
                    }
                }
            }
        }
        _ => Err(anyhow!("Block CRC value must be a CBOR byte string")),
    })?;

    // Confirm we are at the end of the block
    let block_end = block.end_or_else(|| anyhow!("Block has additional items after CRC value"))?;

    // Now check CRC
    if let Some(((crc_value, crc_start), crc_end)) = crc_value {
        let err = anyhow!("Block CRC check failed");

        match crc_type {
            CrcType::CRC16_X25 => {
                let mut digest = X25.digest();
                digest.update(&data[block_start..crc_start]);
                digest.update(&vec![0; crc_end - crc_start]);
                if block_end > crc_end {
                    digest.update(&data[crc_end..block_end]);
                }
                if crc_value != digest.finalize() as u32 {
                    return Err(err);
                }
            }
            CrcType::CRC32_CASTAGNOLI => {
                let mut digest = CASTAGNOLI.digest();
                digest.update(&data[block_start..crc_start]);
                digest.update(&vec![0; crc_end - crc_start]);
                if block_end > crc_end {
                    digest.update(&data[crc_end..block_end]);
                }
                if crc_value != digest.finalize() {
                    return Err(err);
                }
            }
            CrcType::None => return Err(anyhow!("Block has invalid CRC type {}", crc_type as u64)),
        }
    }
    Ok(())
}

pub fn emit_crc_value(crc_type: CrcType, mut data: Vec<u8>) -> Vec<u8> {
    match crc_type {
        CrcType::CRC16_X25 => {
            let crc_value = X25.checksum(&data).to_be_bytes();
            data.truncate(data.len() - crc_value.len());
            data.extend(crc_value)
        }
        CrcType::CRC32_CASTAGNOLI => {
            let crc_value = CASTAGNOLI.checksum(&data).to_be_bytes();
            data.truncate(data.len() - crc_value.len());
            data.extend(crc_value)
        }
        CrcType::None => {}
    }
    data
}
