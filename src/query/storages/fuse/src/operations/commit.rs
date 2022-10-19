//  Copyright 2021 Datafuse Labs.
//
//  Licensed under the Apache License, Version 2.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.

use std::io::ErrorKind;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use backoff::backoff::Backoff;
use backoff::ExponentialBackoffBuilder;
use backon::Retryable;
use common_base::base::ProgressValues;
use common_cache::Cache;
use common_catalog::table::Table;
use common_catalog::table::TableExt;
use common_catalog::table_context::TableContext;
use common_datavalues::DataSchema;
use common_exception::ErrorCode;
use common_exception::Result;
use common_fuse_meta::caches::CacheManager;
use common_fuse_meta::meta::ClusterKey;
use common_fuse_meta::meta::Location;
use common_fuse_meta::meta::SegmentInfo;
use common_fuse_meta::meta::Statistics;
use common_fuse_meta::meta::TableSnapshot;
use common_fuse_meta::meta::Versioned;
use common_meta_app::schema::TableInfo;
use common_meta_app::schema::TableStatistics;
use common_meta_app::schema::UpdateTableMetaReq;
use common_meta_types::MatchSeq;
use opendal::Operator;
use tracing::debug;
use tracing::info;
use tracing::warn;
use uuid::Uuid;

use crate::io::write_meta;
use crate::io::SegmentsIO;
use crate::io::TableMetaLocationGenerator;
use crate::operations::mutation::AbortOperation;
use crate::operations::AppendOperationLogEntry;
use crate::operations::TableOperationLog;
use crate::statistics;
use crate::statistics::merge_statistics;
use crate::FuseTable;
use crate::OPT_KEY_LEGACY_SNAPSHOT_LOC;
use crate::OPT_KEY_SNAPSHOT_LOCATION;

const OCC_DEFAULT_BACKOFF_INIT_DELAY_MS: Duration = Duration::from_millis(5);
const OCC_DEFAULT_BACKOFF_MAX_DELAY_MS: Duration = Duration::from_millis(20 * 1000);
const OCC_DEFAULT_BACKOFF_MAX_ELAPSED_MS: Duration = Duration::from_millis(120 * 1000);
const MAX_RETRIES: u64 = 10;

impl FuseTable {
    pub async fn do_commit(
        &self,
        ctx: Arc<dyn TableContext>,
        operation_log: TableOperationLog,
        overwrite: bool,
    ) -> Result<()> {
        let mut tbl = self;
        let mut latest: Arc<dyn Table>;

        let mut retry_times = 0;

        // The initial retry delay in millisecond. By default,  it is 5 ms.
        let init_delay = OCC_DEFAULT_BACKOFF_INIT_DELAY_MS;

        // The maximum  back off delay in millisecond, once the retry interval reaches this value, it stops increasing.
        // By default, it is 20 seconds.
        let max_delay = OCC_DEFAULT_BACKOFF_MAX_DELAY_MS;

        // The maximum elapsed time after the occ starts, beyond which there will be no more retries.
        // By default, it is 2 minutes
        let max_elapsed = OCC_DEFAULT_BACKOFF_MAX_ELAPSED_MS;

        // TODO(xuanwo): move to backon instead.
        //
        // To simplify the settings, using fixed common values for randomization_factor and multiplier
        let mut backoff = ExponentialBackoffBuilder::new()
            .with_initial_interval(init_delay)
            .with_max_interval(max_delay)
            .with_randomization_factor(0.5)
            .with_multiplier(2.0)
            .with_max_elapsed_time(Some(max_elapsed))
            .build();

        let transient = self.transient();
        loop {
            match tbl.try_commit(ctx.clone(), &operation_log, overwrite).await {
                Ok(_) => {
                    break {
                        if transient {
                            // Removes historical data, if table is transient
                            warn!(
                                "transient table detected, purging historical data. ({})",
                                tbl.table_info.ident
                            );

                            let latest = tbl.refresh(ctx.as_ref()).await?;
                            tbl = FuseTable::try_from_table(latest.as_ref())?;

                            let keep_last_snapshot = true;
                            if let Err(e) = tbl.do_gc(&ctx, keep_last_snapshot).await {
                                // Errors of GC, if any, are ignored, since GC task can be picked up
                                warn!(
                                    "GC of transient table not success (this is not a permanent error). the error : {}",
                                    e
                                );
                            } else {
                                info!("GC of transient table done");
                            }
                        }
                        Ok(())
                    };
                }
                Err(e) if self::utils::is_error_recoverable(&e, transient) => match backoff
                    .next_backoff()
                {
                    Some(d) => {
                        let name = tbl.table_info.name.clone();
                        debug!(
                            "got error TableVersionMismatched, tx will be retried {} ms later. table name {}, identity {}",
                            d.as_millis(),
                            name.as_str(),
                            tbl.table_info.ident
                        );
                        common_base::base::tokio::time::sleep(d).await;
                        latest = tbl.refresh(ctx.as_ref()).await?;
                        tbl = FuseTable::try_from_table(latest.as_ref())?;
                        retry_times += 1;
                        continue;
                    }
                    None => {
                        info!("aborting operations");
                        let _ =
                            self::utils::abort_operations(self.get_operator(), operation_log).await;
                        break Err(ErrorCode::OCCRetryFailure(format!(
                            "can not fulfill the tx after retries({} times, {} ms), aborted. table name {}, identity {}",
                            retry_times,
                            Instant::now()
                                .duration_since(backoff.start_time)
                                .as_millis(),
                            tbl.table_info.name.as_str(),
                            tbl.table_info.ident,
                        )));
                    }
                },
                Err(e) => break Err(e),
            }
        }
    }

    #[inline]
    pub async fn try_commit(
        &self,
        ctx: Arc<dyn TableContext>,
        operation_log: &TableOperationLog,
        overwrite: bool,
    ) -> Result<()> {
        let prev = self.read_table_snapshot(ctx.clone()).await?;
        let prev_version = self.snapshot_format_version().await?;
        let prev_timestamp = prev.as_ref().and_then(|v| v.timestamp);
        let schema = self.table_info.meta.schema.as_ref().clone();
        let (segments, summary) = Self::merge_append_operations(operation_log)?;

        let progress_values = ProgressValues {
            rows: summary.row_count as usize,
            bytes: summary.uncompressed_byte_size as usize,
        };
        ctx.get_write_progress().incr(&progress_values);

        let segments = segments
            .into_iter()
            .map(|loc| (loc, SegmentInfo::VERSION))
            .collect();

        let new_snapshot = if overwrite {
            TableSnapshot::new(
                Uuid::new_v4(),
                &prev_timestamp,
                prev.as_ref().map(|v| (v.snapshot_id, prev_version)),
                schema,
                summary,
                segments,
                self.cluster_key_meta.clone(),
            )
        } else {
            Self::merge_table_operations(
                self.table_info.meta.schema.as_ref(),
                prev,
                prev_version,
                segments,
                summary,
                self.cluster_key_meta.clone(),
            )?
        };

        let mut new_table_meta = self.get_table_info().meta.clone();
        // update statistics
        new_table_meta.statistics = TableStatistics {
            number_of_rows: new_snapshot.summary.row_count,
            data_bytes: new_snapshot.summary.uncompressed_byte_size,
            compressed_data_bytes: new_snapshot.summary.compressed_byte_size,
            index_data_bytes: new_snapshot.summary.index_size,
        };

        FuseTable::commit_to_meta_server(
            ctx.as_ref(),
            &self.table_info,
            &self.meta_location_generator,
            new_snapshot,
            &self.operator,
        )
        .await
    }

    fn merge_table_operations(
        schema: &DataSchema,
        previous: Option<Arc<TableSnapshot>>,
        prev_version: u64,
        mut new_segments: Vec<Location>,
        statistics: Statistics,
        cluster_key_meta: Option<ClusterKey>,
    ) -> Result<TableSnapshot> {
        // 1. merge stats with previous snapshot, if any
        let stats = if let Some(snapshot) = &previous {
            let summary = &snapshot.summary;
            statistics::merge_statistics(&statistics, summary)?
        } else {
            statistics
        };
        let prev_snapshot_id = previous.as_ref().map(|v| (v.snapshot_id, prev_version));
        let prev_snapshot_timestamp = previous.as_ref().and_then(|v| v.timestamp);

        // 2. merge segment locations with previous snapshot, if any
        if let Some(snapshot) = &previous {
            let mut segments = snapshot.segments.clone();
            new_segments.append(&mut segments)
        };

        let new_snapshot = TableSnapshot::new(
            Uuid::new_v4(),
            &prev_snapshot_timestamp,
            prev_snapshot_id,
            schema.clone(),
            stats,
            new_segments,
            cluster_key_meta,
        );
        Ok(new_snapshot)
    }

    pub async fn commit_to_meta_server(
        ctx: &dyn TableContext,
        table_info: &TableInfo,
        location_generator: &TableMetaLocationGenerator,
        snapshot: TableSnapshot,
        operator: &Operator,
    ) -> Result<()> {
        let snapshot_location = location_generator
            .snapshot_location_from_uuid(&snapshot.snapshot_id, snapshot.format_version())?;

        // 1. write down snapshot
        write_meta(operator, &snapshot_location, &snapshot).await?;

        // 2. prepare table meta
        let mut new_table_meta = table_info.meta.clone();
        // 2.1 set new snapshot location
        new_table_meta.options.insert(
            OPT_KEY_SNAPSHOT_LOCATION.to_owned(),
            snapshot_location.clone(),
        );
        // remove legacy options
        self::utils::remove_legacy_options(&mut new_table_meta.options);

        // 2.2 setup table statistics
        let stats = &snapshot.summary;
        // update statistics
        new_table_meta.statistics = TableStatistics {
            number_of_rows: stats.row_count,
            data_bytes: stats.uncompressed_byte_size,
            compressed_data_bytes: stats.compressed_byte_size,
            index_data_bytes: stats.index_size,
        };

        // 3. prepare the request
        let catalog = ctx.get_catalog(&table_info.meta.catalog)?;
        let table_id = table_info.ident.table_id;
        let table_version = table_info.ident.seq;

        let req = UpdateTableMetaReq {
            table_id,
            seq: MatchSeq::Exact(table_version),
            new_table_meta,
        };

        // 3. let's roll
        let tenant = ctx.get_tenant();
        let db_name = ctx.get_current_database();
        let reply = catalog.update_table_meta(&tenant, &db_name, req).await;
        match reply {
            Ok(_) => {
                if let Some(snapshot_cache) = CacheManager::instance().get_table_snapshot_cache() {
                    let cache = &mut snapshot_cache.write();
                    cache.put(snapshot_location.clone(), Arc::new(snapshot));
                }
                // try keep a hit file of last snapshot
                Self::write_last_snapshot_hint(operator, location_generator, snapshot_location)
                    .await;
                Ok(())
            }
            Err(e) => {
                // commit snapshot to meta server failed, try to delete it.
                // "major GC" will collect this, if deletion failure (even after DAL retried)
                let _ = operator.object(&snapshot_location).delete().await;
                Err(e)
            }
        }
    }

    pub fn merge_append_operations(
        append_log_entries: &[AppendOperationLogEntry],
    ) -> Result<(Vec<String>, Statistics)> {
        let iter = append_log_entries
            .iter()
            .map(|entry| (&entry.segment_location, entry.segment_info.as_ref()));
        FuseTable::merge_segments(iter)
    }

    pub fn merge_segments<'a, T>(
        mut segments: impl Iterator<Item = (&'a T, &'a SegmentInfo)>,
    ) -> Result<(Vec<T>, Statistics)>
    where T: Clone + 'a {
        let len_hint = segments
            .size_hint()
            .1
            .unwrap_or_else(|| segments.size_hint().0);
        let (s, seg_locs) = segments.try_fold(
            (Statistics::default(), Vec::with_capacity(len_hint)),
            |(mut acc, mut seg_acc), (location, segment_info)| {
                let stats = &segment_info.summary;
                acc.row_count += stats.row_count;
                acc.block_count += stats.block_count;
                acc.uncompressed_byte_size += stats.uncompressed_byte_size;
                acc.compressed_byte_size += stats.compressed_byte_size;
                acc.index_size = stats.index_size;
                acc.col_stats = if acc.col_stats.is_empty() {
                    stats.col_stats.clone()
                } else {
                    statistics::reduce_block_statistics(&[&acc.col_stats, &stats.col_stats])?
                };
                seg_acc.push(location.clone());
                Ok::<_, ErrorCode>((acc, seg_acc))
            },
        )?;

        Ok((seg_locs, s))
    }

    // Left a hint file which indicates the location of the latest snapshot
    async fn write_last_snapshot_hint(
        operator: &Operator,
        location_generator: &TableMetaLocationGenerator,
        last_snapshot_path: String,
    ) {
        // Just try our best to write down the hint file of last snapshot
        // - will retry in the case of temporary failure
        // but
        // - errors are ignored if writing is eventually failed
        // - errors (if any) will not be propagated to caller
        // - "data race" ignored
        //   if multiple different versions of hints are written concurrently
        //   it is NOT guaranteed that the latest version will be kept

        let hint_path = location_generator.gen_last_snapshot_hint_location();
        let last_snapshot_path = {
            let operator_meta_data = operator.metadata();
            let storage_prefix = operator_meta_data.root();
            format!("{}{}", storage_prefix, last_snapshot_path)
        };

        let object = operator.object(&hint_path);
        { || object.write(last_snapshot_path.as_bytes()) }
            .retry(backon::ExponentialBackoff::default())
            .when(|err| err.kind() == ErrorKind::Interrupted)
            .notify(|err, dur| {
                warn!(
                    "fuse table write_last_snapshot_hint retry after {}s for error {:?}",
                    dur.as_secs(),
                    err
                )
            })
            .await
            .unwrap_or_else(|e| {
                warn!("write last snapshot hint failure. {}", e);
            })
    }

    pub async fn commit_mutation(
        &self,
        ctx: Arc<dyn TableContext>,
        snapshot: Arc<TableSnapshot>,
        mut segments: Vec<Location>,
        mut summary: Statistics,
        abort_operation: AbortOperation,
    ) -> Result<()> {
        let mut table = self;
        let mut latest: Arc<dyn Table>;
        let mut retries = 0;
        let mut latest_snapshot;
        let mut base_snapshot = snapshot;
        let mut current_table_info = &self.table_info;

        while retries < MAX_RETRIES {
            let mut snapshot_tobe_committed = TableSnapshot::from_previous(base_snapshot.as_ref());
            snapshot_tobe_committed.segments = segments.clone();
            snapshot_tobe_committed.summary = summary.clone();
            match Self::commit_to_meta_server(
                ctx.as_ref(),
                current_table_info,
                &self.meta_location_generator,
                snapshot_tobe_committed,
                &self.operator,
            )
            .await
            {
                Err(e) if e.code() == ErrorCode::table_version_mismatched_code() => {
                    latest = table.refresh(ctx.as_ref()).await?;
                    table = FuseTable::try_from_table(latest.as_ref())?;

                    latest_snapshot =
                        table
                            .read_table_snapshot(ctx.clone())
                            .await?
                            .ok_or_else(|| {
                                ErrorCode::LogicalError(
                                    "mutation meets empty snapshot during conflict reconciliation",
                                )
                            })?;
                    current_table_info = &table.table_info;

                    if latest_snapshot.segments.len() < base_snapshot.segments.len() {
                        abort_operation
                            .abort(ctx.clone(), self.operator.clone())
                            .await?;
                        return Err(ErrorCode::StorageOther(
                            "mutation conflicts, concurrent mutation detected while committing segment compaction operation",
                        ));
                    }

                    // Check if there is only insertion during the operation.
                    let mut new_segments = latest_snapshot.segments.clone();
                    match MutatorConflictDetector::detect_conflicts(
                        base_snapshot.as_ref(),
                        latest_snapshot.as_ref(),
                    ) {
                        Conflict::Unresolvable => {
                            abort_operation
                                .abort(ctx.clone(), self.operator.clone())
                                .await?;
                            return Err(ErrorCode::StorageOther(
                                "mutation conflicts, concurrent mutation detected while committing segment compaction operation",
                            ));
                        }
                        Conflict::ResolvableAppend(range_of_newly_append) => {
                            // for resolvable append only operation, the range returned always begin with 0
                            new_segments.truncate(range_of_newly_append.len());
                        }
                    }

                    info!("resolvable conflicts detected");
                    // Read all segments information in parallel.
                    let fuse_segment_io = SegmentsIO::create(ctx.clone(), table.operator.clone());
                    let results = fuse_segment_io.read_segments(&new_segments).await?;
                    for result in results.iter() {
                        let segment = result.clone()?;
                        summary = merge_statistics(&summary, &segment.summary)?;
                    }
                    new_segments.extend(segments.clone());
                    segments = new_segments;
                    base_snapshot = latest_snapshot;
                    retries += 1;
                }
                Err(e) => return Err(e),
                Ok(_) => return Ok(()),
            }
        }

        abort_operation
            .abort(ctx.clone(), self.operator.clone())
            .await?;
        Err(ErrorCode::StorageOther(format!(
            "commit mutation failed after {} retries",
            retries
        )))
    }
}

pub enum Conflict {
    Unresolvable,
    // resolvable conflicts with append only operation
    // the range embedded is the range of segments that are appended in the latest snapshot
    ResolvableAppend(Range<usize>),
}

// wraps a namespace, to clarify the who is detecting conflict
pub struct MutatorConflictDetector;

impl MutatorConflictDetector {
    // detects conflicts, as a mutator, working on the base snapshot, with latest snapshot
    pub fn detect_conflicts(base: &TableSnapshot, latest: &TableSnapshot) -> Conflict {
        let base_segments = &base.segments;
        let latest_segments = &latest.segments;

        let base_segments_len = base_segments.len();
        let latest_segments_len = latest_segments.len();

        if latest_segments_len >= base_segments_len
            && base_segments[0..base_segments_len]
                == latest_segments[(latest_segments_len - base_segments_len)..latest_segments_len]
        {
            // note (latest_segments_len == base_segments_len indicate) indicates that the latest
            // snapshot is generated by non-data related operations, like `alter table add column`
            // which can be merged
            Conflict::ResolvableAppend(0..(latest_segments_len - base_segments_len))
        } else {
            Conflict::Unresolvable
        }
    }
}

mod utils {
    use std::collections::BTreeMap;

    use super::*;
    #[inline]
    pub async fn abort_operations(
        operator: Operator,
        operation_log: TableOperationLog,
    ) -> Result<()> {
        for entry in operation_log {
            for block in &entry.segment_info.blocks {
                let block_location = &block.location.0;
                // if deletion operation failed (after DAL retried)
                // we just left them there, and let the "major GC" collect them
                let _ = operator.object(block_location).delete().await;
                if let Some(index) = &block.bloom_filter_index_location {
                    let _ = operator.object(&index.0).delete().await;
                }
            }
            let _ = operator.object(&entry.segment_location).delete().await;
        }
        Ok(())
    }

    #[inline]
    pub fn is_error_recoverable(e: &ErrorCode, is_table_transient: bool) -> bool {
        let code = e.code();
        code == ErrorCode::table_version_mismatched_code()
            || (is_table_transient && code == ErrorCode::storage_not_found_code())
    }

    // check if there are any fuse table legacy options
    pub fn remove_legacy_options(table_options: &mut BTreeMap<String, String>) {
        table_options.remove(OPT_KEY_LEGACY_SNAPSHOT_LOC);
    }
}
