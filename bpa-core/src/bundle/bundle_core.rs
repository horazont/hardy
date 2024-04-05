use super::*;
use std::collections::HashMap;

pub struct Bundle {
    pub metadata: Option<Metadata>,
    pub primary: PrimaryBlock,
    pub blocks: HashMap<u64, Block>,
}

impl Bundle {
    pub fn parse(data: &[u8]) -> Result<(Self, bool), anyhow::Error> {
        let ((bundle, valid), consumed) = cbor::decode::parse_value(data, |value, tags| {
            if let cbor::decode::Value::Array(blocks) = value {
                if !tags.is_empty() {
                    log::info!("Parsing bundle with tags");
                }
                parse_bundle_blocks(data, blocks)
            } else {
                Err(anyhow!("Bundle is not a CBOR array"))
            }
        })?;
        if valid && consumed < data.len() {
            return Err(anyhow!(
                "Bundle has additional data after end of CBOR array"
            ));
        }
        Ok((bundle, valid))
    }
}

fn parse_bundle_blocks(
    data: &[u8],
    mut blocks: cbor::decode::Array,
) -> Result<(Bundle, bool), anyhow::Error> {
    // Parse Primary block
    let (primary, valid) = blocks.try_parse_item(|value, block_start, tags| {
        if let cbor::decode::Value::Array(a) = value {
            if !tags.is_empty() {
                log::info!("Parsing primary block with tags");
            }
            PrimaryBlock::parse(data, a, block_start)
        } else {
            Err(anyhow!("Bundle primary block is not a CBOR array"))
        }
    })?;

    let (bundle_blocks, valid) = if valid {
        // Parse other blocks
        match parse_extension_blocks(data, blocks) {
            Ok(bundle_blocks) => (bundle_blocks, true),
            Err(e) => {
                // Don't return an Err, we need to return Ok(invalid)
                log::info!("Extension block parsing failed: {}", e);
                (HashMap::new(), false)
            }
        }
    } else {
        (HashMap::new(), false)
    };

    Ok((
        Bundle {
            metadata: None,
            primary,
            blocks: bundle_blocks,
        },
        valid,
    ))
}

fn parse_extension_blocks(
    data: &[u8],
    mut blocks: cbor::decode::Array,
) -> Result<HashMap<u64, Block>, anyhow::Error> {
    // Use an intermediate vector so we can check the payload was the last item
    let mut extension_blocks = Vec::new();
    let extension_map = loop {
        if let Some((block_number, block)) =
            blocks.try_parse_item(|value, block_start, tags| match value {
                cbor::decode::Value::Array(a) => {
                    if !tags.is_empty() {
                        log::info!("Parsing extension block with tags");
                    }
                    Ok(Some(Block::parse(data, a, block_start)?))
                }
                cbor::decode::Value::End(_) => Ok(None),
                _ => Err(anyhow!("Bundle extension block is not a CBOR array")),
            })?
        {
            extension_blocks.push((block_number, block));
        } else {
            // Check the last block is the payload
            let Some((block_number, payload)) = extension_blocks.last() else {
                return Err(anyhow!("Bundle has no payload block"));
            };

            if let BlockType::Payload = payload.block_type {
                if *block_number != 1 {
                    return Err(anyhow!("Bundle payload block must be block number 1"));
                }
            } else {
                return Err(anyhow!("Final block of bundle is not a payload block"));
            }

            // Compose hashmap
            let mut map = HashMap::new();
            for (block_number, block) in extension_blocks {
                if map.insert(block_number, block).is_some() {
                    return Err(anyhow!(
                        "Bundle has more than one block with block number {}",
                        block_number
                    ));
                }
            }
            break map;
        }
    };

    // Check for duplicates

    Ok(extension_map)
}
