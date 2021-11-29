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
//

use std::any::Any;
use std::collections::HashSet;
use std::sync::Arc;

use common_datablocks::DataBlock;
use common_datavalues::columns::DataColumn;
use common_datavalues::DataSchema;
use common_exception::Result;
use common_infallible::RwLock;
use common_meta_types::TableInfo;
use common_planners::Extras;
use common_planners::Partitions;
use common_planners::ReadDataSourcePlan;
use common_planners::Statistics;
use common_planners::TruncateTablePlan;
use common_streams::DataBlockStream;
use common_streams::SendableDataBlockStream;
use futures::stream::StreamExt;

use crate::catalogs::Table;
use crate::sessions::QueryContext;
use crate::storages::memory::MemoryTableStream;
use crate::storages::StorageContext;

pub struct MemoryTable {
    table_info: TableInfo,
    blocks: Arc<RwLock<Vec<DataBlock>>>,
}

impl MemoryTable {
    pub fn try_create(ctx: StorageContext, table_info: TableInfo) -> Result<Box<dyn Table>> {
        let table_id = &table_info.ident.table_id;
        let blocks = {
            let mut in_mem_data = ctx.in_memory_data.write();
            let x = in_mem_data.get(table_id);
            match x {
                None => {
                    let blocks = Arc::new(RwLock::new(vec![]));
                    in_mem_data.insert(*table_id, blocks.clone());
                    blocks
                }
                Some(blocks) => blocks.clone(),
            }
        };

        let table = Self { table_info, blocks };
        Ok(Box::new(table))
    }
}

#[async_trait::async_trait]
impl Table for MemoryTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn get_table_info(&self) -> &TableInfo {
        &self.table_info
    }

    fn benefit_column_prune(&self) -> bool {
        true
    }

    async fn read_partitions(
        &self,
        ctx: Arc<QueryContext>,
        push_downs: Option<Extras>,
    ) -> Result<(Statistics, Partitions)> {
        let blocks = self.blocks.read();

        let statistics = if let Some(Extras {
            projection: Some(prj),
            ..
        }) = push_downs
        {
            let proj_cols = HashSet::<usize>::from_iter(prj);
            blocks
                .iter()
                .fold(Statistics::default(), |mut stats, block| {
                    stats.read_rows += block.num_rows() as usize;
                    stats.read_bytes += (0..block.num_columns())
                        .into_iter()
                        .collect::<Vec<usize>>()
                        .iter()
                        .filter(|cid| proj_cols.contains(&(**cid as usize)))
                        .map(|cid| block.columns()[*cid].get_array_memory_size() as u64)
                        .sum::<u64>() as usize;

                    stats
                })
        } else {
            let rows = blocks.iter().map(|block| block.num_rows()).sum();
            let bytes = blocks.iter().map(|block| block.memory_size()).sum();

            Statistics::new_exact(rows, bytes)
        };

        let parts = crate::table_functions::generate_block_parts(
            0,
            ctx.get_settings().get_max_threads()? as u64,
            blocks.len() as u64,
        );
        Ok((statistics, parts))
    }

    async fn read(
        &self,
        ctx: Arc<QueryContext>,
        plan: &ReadDataSourcePlan,
    ) -> Result<SendableDataBlockStream> {
        let push_downs = &plan.push_downs;

        let blocks = if let Some(Extras {
            projection: Some(prj),
            ..
        }) = push_downs
        {
            let mut pruned_blocks = Vec::with_capacity(prj.len());
            let schema = Arc::new(self.table_info.schema().project(prj.clone()));
            let raw_blocks = self.blocks.read().clone();

            for raw_block in raw_blocks {
                let raw_columns = raw_block.columns();
                let columns: Vec<DataColumn> =
                    prj.iter().map(|idx| raw_columns[*idx].clone()).collect();

                pruned_blocks.push(DataBlock::create(schema.clone(), columns))
            }

            pruned_blocks
        } else {
            self.blocks.read().clone()
        };

        Ok(Box::pin(MemoryTableStream::try_create(ctx, blocks)?))
    }

    async fn append_data(
        &self,
        _ctx: Arc<QueryContext>,
        mut stream: SendableDataBlockStream,
    ) -> Result<SendableDataBlockStream> {
        while let Some(block) = stream.next().await {
            let block = block?;
            let mut blocks = self.blocks.write();
            blocks.push(block);
        }
        Ok(Box::pin(DataBlockStream::create(
            std::sync::Arc::new(DataSchema::empty()),
            None,
            vec![],
        )))
    }

    async fn truncate(
        &self,
        _ctx: Arc<QueryContext>,
        _truncate_plan: TruncateTablePlan,
    ) -> Result<()> {
        let mut blocks = self.blocks.write();
        blocks.clear();
        Ok(())
    }
}
