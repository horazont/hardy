use super::*;
use anyhow::anyhow;
use base64::prelude::*;
use hardy_bpa_core::{
    async_trait,
    bundle::{self, HopInfo},
    storage::MetadataStorage,
};
use hardy_cbor as cbor;
use std::{collections::HashMap, fs::create_dir_all, path::PathBuf, sync::Arc};

pub struct Storage {
    connection: tokio::sync::Mutex<rusqlite::Connection>,
}

impl Storage {
    pub fn init(
        config: &HashMap<String, config::Value>,
        mut upgrade: bool,
    ) -> Result<std::sync::Arc<dyn MetadataStorage>, anyhow::Error> {
        let db_dir: String = config.get("db_dir").map_or_else(
            || {
                directories::ProjectDirs::from("dtn", "Hardy", built_info::PKG_NAME).map_or_else(
                    || Err(anyhow!("Failed to resolve local store directory")),
                    |project_dirs| {
                        Ok(project_dirs.cache_dir().to_string_lossy().to_string())
                        // Lin: /home/alice/.store/barapp
                        // Win: C:\Users\Alice\AppData\Local\Foo Corp\Bar App\store
                        // Mac: /Users/Alice/Library/stores/com.Foo-Corp.Bar-App
                    },
                )
            },
            |v| {
                v.clone()
                    .into_string()
                    .map_err(|e| anyhow!("'db_dir' is not a string value: {}!", e))
            },
        )?;

        // Compose DB name
        let file_path = [&db_dir, "metadata.db"].iter().collect::<PathBuf>();

        // Ensure directory exists
        create_dir_all(file_path.parent().unwrap())?;

        // Attempt to open existing database first
        let mut connection = match rusqlite::Connection::open_with_flags(
            &file_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(conn) => conn,
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ffi::ErrorCode::CannotOpen,
                    extended_code: _,
                },
                _,
            )) => {
                // Create database
                upgrade = true;
                rusqlite::Connection::open_with_flags(
                    &file_path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                        | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?
            }
            Err(e) => Err(e)?,
        };

        // Migrate the database to the latest schema
        migrate::migrate(&mut connection, upgrade)?;

        // Mark all existing bundles as unconfirmed
        connection.execute_batch(
            r#"
            INSERT OR IGNORE INTO unconfirmed_bundles (bundle_id)
            SELECT id FROM bundles;

            CREATE TEMPORARY TABLE restart_bundles (
                bundle_id INTEGER UNIQUE NOT NULL
            ) STRICT;"#,
        )?;

        Ok(Arc::new(Storage {
            connection: tokio::sync::Mutex::new(connection),
        }))
    }
}

fn encode_eid(eid: &bundle::Eid) -> Result<rusqlite::types::Value, anyhow::Error> {
    match eid {
        bundle::Eid::Null => Ok(rusqlite::types::Value::Null),
        _ => Ok(rusqlite::types::Value::Blob(cbor::encode::emit(eid))),
    }
}

fn decode_eid(
    row: &rusqlite::Row,
    idx: impl rusqlite::RowIndex,
) -> Result<bundle::Eid, anyhow::Error> {
    match row.get_ref(idx)? {
        rusqlite::types::ValueRef::Blob(b) => cbor::decode::parse(b),
        rusqlite::types::ValueRef::Null => Ok(bundle::Eid::Null),
        _ => Err(anyhow!("EID encoded as unusual sqlite type")),
    }
}

// Quick helper for type conversion
#[inline]
fn as_u64(v: i64) -> u64 {
    v as u64
}

fn unpack_bundles(
    mut rows: rusqlite::Rows,
) -> Result<Vec<(i64, bundle::Metadata, bundle::Bundle)>, anyhow::Error> {
    /* Expected query MUST look like:
           0:  bundles.id,
           1:  bundles.status,
           2:  bundles.storage_name,
           3:  bundles.hash,
           4:  bundles.received_at,
           5:  bundles.flags,
           6:  bundles.crc_type,
           7:  bundles.source,
           8:  bundles.destination,
           9:  bundles.report_to,
           10: bundles.creation_time,
           11: bundles.creation_seq_num,
           12: bundles.lifetime,
           13: bundles.fragment_offset,
           14: bundles.fragment_total_len,
           15: bundles.previous_node,
           16: bundles.age,
           17: bundles.hop_count,
           18: bundles.hop_limit,
           19: bundle_blocks.block_num,
           20: bundle_blocks.block_type,
           21: bundle_blocks.block_flags,
           22: bundle_blocks.block_crc_type,
           23: bundle_blocks.data_offset,
           24: bundle_blocks.data_len
    */

    let mut bundles = Vec::new();
    let mut row_result = rows.next()?;
    while let Some(mut row) = row_result {
        let bundle_id: i64 = row.get(0)?;
        let metadata = bundle::Metadata {
            status: as_u64(row.get(1)?).try_into()?,
            storage_name: row.get(2)?,
            hash: BASE64_STANDARD.decode(row.get::<usize, String>(3)?)?,
            received_at: row.get(4)?,
        };

        let fragment_info = {
            let offset: i64 = row.get(13)?;
            let total_len: i64 = row.get(14)?;
            if offset == -1 && total_len == -1 {
                None
            } else {
                Some(bundle::FragmentInfo {
                    offset: offset as u64,
                    total_len: total_len as u64,
                })
            }
        };

        let mut bundle = bundle::Bundle {
            id: bundle::BundleId {
                source: decode_eid(row, 7)?,
                timestamp: bundle::CreationTimestamp {
                    creation_time: as_u64(row.get(10)?),
                    sequence_number: as_u64(row.get(11)?),
                },
                fragment_info,
            },
            flags: as_u64(row.get(5)?).into(),
            crc_type: as_u64(row.get(6)?).try_into()?,
            destination: decode_eid(row, 8)?,
            report_to: decode_eid(row, 9)?,
            lifetime: as_u64(row.get(12)?),
            blocks: HashMap::new(),
            previous_node: match row.get_ref(15)? {
                rusqlite::types::ValueRef::Null => None,
                rusqlite::types::ValueRef::Blob(b) => Some(cbor::decode::parse(b)?),
                _ => return Err(anyhow!("EID encoded as unusual sqlite type")),
            },
            age: row
                .get::<usize, Option<i64>>(16)?
                .and_then(|v| Some(v as u64)),
            hop_count: match row.get_ref(17)? {
                rusqlite::types::ValueRef::Null => None,
                rusqlite::types::ValueRef::Integer(i) => Some(HopInfo {
                    count: i as usize,
                    limit: row.get::<usize, i64>(18)? as usize,
                }),
                _ => return Err(anyhow!("EID encoded as unusual sqlite type")),
            },
        };

        loop {
            let block_number = as_u64(row.get(19)?);
            let block = bundle::Block {
                block_type: as_u64(row.get(20)?).try_into()?,
                flags: as_u64(row.get(21)?).into(),
                crc_type: as_u64(row.get(22)?).try_into()?,
                data_offset: as_u64(row.get(23)?) as usize,
                data_len: as_u64(row.get(24)?) as usize,
            };

            if bundle.blocks.insert(block_number, block).is_some() {
                return Err(anyhow!("Duplicate block number in DB!"));
            }

            row_result = rows.next()?;
            row = match row_result {
                None => break,
                Some(row) => row,
            };

            if row.get::<usize, i64>(0)? != bundle_id {
                break;
            }
        }

        bundles.push((bundle_id, metadata, bundle));
    }
    Ok(bundles)
}

#[async_trait]
impl MetadataStorage for Storage {
    fn check_orphans(
        &self,
        f: &mut dyn FnMut(bundle::Metadata, bundle::Bundle) -> Result<bool, anyhow::Error>,
    ) -> Result<(), anyhow::Error> {
        // Loop through subsets of 16 bundles, so we don't fill all memory
        loop {
            let bundles = unpack_bundles(
                self.connection
                    .blocking_lock()
                    .prepare_cached(
                        r#"WITH subset AS (
                            SELECT 
                                id,
                                status,
                                storage_name,
                                hash,
                                received_at,
                                flags,
                                crc_type,
                                source,
                                destination,
                                report_to,
                                creation_time,
                                creation_seq_num,
                                lifetime,                    
                                fragment_offset,
                                fragment_total_len,
                                previous_node,
                                age,
                                hop_count,
                                hop_limit
                            FROM unconfirmed_bundles
                            JOIN bundles ON id = unconfirmed_bundles.bundle_id
                            LIMIT 16
                        )
                        SELECT 
                            subset.*,
                            block_num,
                            block_type,
                            block_flags,
                            block_crc_type,
                            data_offset,
                            data_len
                        FROM subset
                        JOIN bundle_blocks ON bundle_blocks.id = subset.id;"#,
                    )?
                    .query(())?,
            )?;
            if bundles.is_empty() {
                break;
            }

            // Now enumerate the vector outside the query implicit transaction
            for (_bundle_id, metadata, bundle) in bundles {
                if !f(metadata, bundle)? {
                    break;
                }
            }
        }
        Ok(())
    }

    fn restart(
        &self,
        f: &mut dyn FnMut(bundle::Metadata, bundle::Bundle) -> Result<bool, anyhow::Error>,
    ) -> Result<(), anyhow::Error> {
        // Create a temprorary table (because DELETE RETURNING cannot be used as a CTE)
        self.connection
            .blocking_lock()
            .prepare(
                r#"CREATE TEMPORARY TABLE restart_subset (
                    bundle_id INTEGER UNIQUE NOT NULL
                ) STRICT;"#,
            )?
            .execute(())?;

        loop {
            // Loop through subsets of 16 bundles, so we don't fill all memory
            let mut conn = self.connection.blocking_lock();
            let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

            // Grab a subset, ordered by status descending
            trans
                .prepare_cached(
                    r#"INSERT INTO restart_subset (bundle_id)
                            SELECT id
                            FROM restart_bundles
                            JOIN bundles ON bundles.id = restart_bundles.bundle_id
                            ORDER BY bundles.status DESC
                            LIMIT 16;"#,
                )?
                .execute(())?;

            // Remove from restart the subset we are about to process
            if trans
                .prepare_cached(
                    r#"DELETE FROM restart_bundles WHERE bundle_id IN (
                            SELECT bundle_id FROM restart_subset
                        );"#,
                )?
                .execute(())?
                == 0
            {
                break;
            }

            // Now enum the bundles from the subset
            let bundles = unpack_bundles(
                trans
                    .prepare_cached(
                        r#"SELECT 
                            id,
                            status,
                            storage_name,
                            hash,
                            received_at,
                            flags,
                            crc_type,
                            source,
                            destination,
                            report_to,
                            creation_time,
                            creation_seq_num,
                            lifetime,                    
                            fragment_offset,
                            fragment_total_len,
                            previous_node,
                            age,
                            hop_count,
                            hop_limit
                            block_num,
                            block_type,
                            block_flags,
                            block_crc_type,
                            data_offset,
                            data_len
                        FROM restart_subset
                        JOIN bundles ON bundles.id = restart_subset.bundle_id
                        JOIN bundle_blocks ON bundle_blocks.id = restart_subset.bundle_id;"#,
                    )?
                    .query(())?,
            )?;

            // Complete transaction!
            trans.commit()?;
            drop(conn);

            // Now enumerate the vector outside the transaction
            for (_bundle_id, metadata, bundle) in bundles {
                if !f(metadata, bundle)? {
                    break;
                }
            }
        }

        // And finally drop the restart tables - they're no longer required
        self.connection.blocking_lock().execute_batch(
            r#"
                DROP TABLE temp.restart_subset;
                DROP TABLE temp.restart_bundles;"#,
        )?;

        Ok(())
    }

    async fn store(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        let mut conn = self.connection.lock().await;
        let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let (fragment_offset, fragment_total_len) =
            if let Some(fragment_info) = bundle.id.fragment_info {
                (fragment_info.offset as i64, fragment_info.total_len as i64)
            } else {
                (-1, -1)
            };

        let previous_node = match &bundle.previous_node {
            Some(p) => Some(encode_eid(p)?),
            None => None,
        };

        // Insert bundle
        let bundle_id = trans
            .prepare_cached(
                r#"
            INSERT INTO bundles (
                status,
                storage_name,
                hash,
                flags,
                crc_type,
                source,
                destination,
                report_to,
                creation_time,
                creation_seq_num,
                lifetime,
                fragment_offset,
                fragment_total_len,
                previous_node,
                age,
                hop_count,
                hop_limit
                )
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
            RETURNING id;"#,
            )?
            .query_row(
                rusqlite::params![
                    <bundle::BundleStatus as Into<u64>>::into(metadata.status) as i64,
                    &metadata.storage_name,
                    BASE64_STANDARD.encode(&metadata.hash),
                    <bundle::BundleFlags as Into<u64>>::into(bundle.flags) as i64,
                    <bundle::CrcType as Into<u64>>::into(bundle.crc_type) as i64,
                    &encode_eid(&bundle.id.source)?,
                    &encode_eid(&bundle.destination)?,
                    &encode_eid(&bundle.report_to)?,
                    bundle.id.timestamp.creation_time as i64,
                    bundle.id.timestamp.sequence_number as i64,
                    bundle.lifetime as i64,
                    fragment_offset,
                    fragment_total_len,
                    previous_node,
                    bundle.age,
                    bundle.hop_count.and_then(|h| Some(h.count)),
                    bundle.hop_count.and_then(|h| Some(h.limit))
                ],
                |row| Ok(as_u64(row.get(0)?)),
            )?;

        // Insert extension blocks
        let mut block_stmt = trans.prepare_cached(
            r#"
            INSERT INTO bundle_blocks (
                bundle_id,
                block_type,
                block_num,
                block_flags,
                block_crc_type,
                data_offset,
                data_len)
            VALUES (?1,?2,?3,?4,?5,?6);"#,
        )?;
        for (block_num, block) in &bundle.blocks {
            block_stmt.execute((
                bundle_id,
                <bundle::BlockType as Into<u64>>::into(block.block_type) as i64,
                *block_num as i64,
                <bundle::BlockFlags as Into<u64>>::into(block.flags) as i64,
                <bundle::CrcType as Into<u64>>::into(block.crc_type) as i64,
                block.data_offset as i64,
                block.data_len as i64,
            ))?;
        }

        Ok(())
    }

    async fn remove(&self, storage_name: &str) -> Result<bool, anyhow::Error> {
        // Delete
        Ok(self
            .connection
            .lock()
            .await
            .prepare_cached(r#"DELETE FROM bundles WHERE storage_name = ?1;"#)?
            .execute([storage_name])?
            != 0)
    }

    async fn confirm_exists(
        &self,
        storage_name: &str,
        hash: Option<&[u8]>,
    ) -> Result<bool, anyhow::Error> {
        let mut conn = self.connection.lock().await;
        let trans = conn.transaction()?;

        // Check if bundle exists
        let bundle_id: i64 = match if let Some(hash) = hash {
            trans
                .prepare_cached(
                    r#"SELECT id FROM bundles WHERE storage_name = ?1 AND hash = ?2 LIMIT 1;"#,
                )?
                .query_row((storage_name, &BASE64_STANDARD.encode(hash)), |row| {
                    row.get(0)
                })
        } else {
            trans
                .prepare_cached(r#"SELECT id FROM bundles WHERE storage_name = ?1 LIMIT 1;"#)?
                .query_row([storage_name], |row| row.get(0))
        } {
            Ok(bundle_id) => bundle_id,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(false),
            Err(e) => Err(e)?,
        };

        // Remove from unconfirmed set
        if trans
            .prepare_cached(r#"DELETE FROM unconfirmed_bundles WHERE bundle_id = ?1;"#)?
            .execute([bundle_id])?
            > 0
        {
            // Add to restart set
            trans
                .prepare_cached(r#"INSERT INTO restart_bundles (bundle_id) VALUES (?1);"#)?
                .execute([bundle_id])?;
        }
        Ok(true)
    }

    async fn set_bundle_status(
        &self,
        storage_name: &str,
        status: bundle::BundleStatus,
    ) -> Result<(), anyhow::Error> {
        self.connection
            .lock()
            .await
            .prepare_cached(r#"UPDATE bundles SET status = ?1 WHERE storage_name = ?2;"#)?
            .execute((
                <bundle::BundleStatus as Into<u64>>::into(status) as i64,
                storage_name,
            ))?;
        Ok(())
    }
}
