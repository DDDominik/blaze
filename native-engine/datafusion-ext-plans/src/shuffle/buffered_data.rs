// Copyright 2022 The Blaze Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::Write;

use arrow::record_batch::RecordBatch;
use blaze_jni_bridge::jni_call;
use bytesize::ByteSize;
use count_write::CountWrite;
use datafusion::{common::Result, physical_plan::Partitioning};
use datafusion_ext_commons::{
    array_size::ArraySize,
    compute_suggested_batch_size_for_output,
    ds::rdx_tournament_tree::{KeyForRadixTournamentTree, RadixTournamentTree},
    rdxsort::radix_sort_unstable_by_key,
    staging_mem_size_for_partial_sort,
    streams::coalesce_stream::coalesce_batches_unchecked,
};
use itertools::Itertools;
use jni::objects::GlobalRef;

use crate::{
    common::{batch_selection::interleave_batches, ipc_compression::IpcCompressionWriter},
    shuffle::{evaluate_hashes, evaluate_partition_ids, rss::RssWriter},
};

pub struct BufferedData {
    partition_id: usize,
    staging_batches: Vec<RecordBatch>,
    sorted_batches: Vec<RecordBatch>,
    sorted_parts: Vec<Vec<PartitionInBatch>>,
    num_rows: usize,
    staging_mem_used: usize,
    sorted_mem_used: usize,
}

impl BufferedData {
    pub fn new(partition_id: usize) -> Self {
        Self {
            partition_id,
            staging_batches: vec![],
            sorted_batches: vec![],
            sorted_parts: vec![],
            num_rows: 0,
            staging_mem_used: 0,
            sorted_mem_used: 0,
        }
    }

    pub fn drain(&mut self) -> Self {
        std::mem::replace(self, Self::new(self.partition_id))
    }

    pub fn add_batch(&mut self, batch: RecordBatch, partitioning: &Partitioning) -> Result<()> {
        self.num_rows += batch.num_rows();
        self.staging_mem_used += batch.get_array_mem_size();
        self.staging_batches.push(batch);
        if self.staging_mem_used >= staging_mem_size_for_partial_sort() {
            self.flush_staging_batches(partitioning)?;
        }
        Ok(())
    }

    fn flush_staging_batches(&mut self, partitioning: &Partitioning) -> Result<()> {
        log::info!(
            "[partition={}] shuffle buffered data starts partial sort, staging: {}, total: {}, total rows: {}",
            self.partition_id,
            ByteSize(self.staging_mem_used as u64),
            ByteSize(self.mem_used() as u64),
            self.num_rows,
        );
        let staging_batches = std::mem::take(&mut self.staging_batches);
        self.staging_mem_used = 0;

        let (parts, sorted_batch) = sort_batches_by_partition_id(staging_batches, partitioning)?;

        self.sorted_mem_used +=
            sorted_batch.get_array_mem_size() + parts.len() * size_of::<PartitionInBatch>();
        self.sorted_batches.push(sorted_batch);
        self.sorted_parts.push(parts);
        Ok(())
    }

    // write buffered data to spill/target file, returns uncompressed size and
    // offsets to each partition
    pub fn write<W: Write>(self, mut w: W, partitioning: &Partitioning) -> Result<Vec<u64>> {
        let partition_id = self.partition_id;
        log::info!(
            "[partition={partition_id}] draining all buffered data, total_mem={}",
            self.mem_used()
        );

        if self.num_rows == 0 {
            return Ok(vec![0; partitioning.partition_count() + 1]);
        }
        let mut writer = IpcCompressionWriter::new(CountWrite::from(&mut w));
        let mut offsets = vec![];
        let mut offset = 0;
        let mut iter = self.into_sorted_batches(partitioning)?;

        while (iter.cur_part_id() as usize) < partitioning.partition_count() {
            let cur_part_id = iter.cur_part_id();
            while offsets.len() <= cur_part_id as usize {
                offsets.push(offset); // fill offsets of empty partitions
            }

            // write all batches with this part id
            while iter.cur_part_id() == cur_part_id {
                let batch = iter.next_batch();
                writer.write_batch(batch.num_rows(), batch.columns())?;
            }
            writer.finish_current_buf()?;
            offset = writer.inner().count();
            offsets.push(offset);
        }
        while offsets.len() <= partitioning.partition_count() {
            offsets.push(offset); // fill offsets of empty partitions
        }
        let compressed_size = offsets.last().cloned().unwrap_or_default();

        log::info!("[partition={partition_id}] all buffered data drained, compressed_size={compressed_size}");
        Ok(offsets)
    }

    // write buffered data to rss, returns uncompressed size
    pub fn write_rss(
        self,
        rss_partition_writer: GlobalRef,
        partitioning: &Partitioning,
    ) -> Result<()> {
        let partition_id = self.partition_id;
        log::info!(
            "[partition={partition_id}] draining all buffered data to rss, total_mem={}",
            self.mem_used()
        );

        if self.num_rows == 0 {
            return Ok(());
        }
        let mut iter = self.into_sorted_batches(partitioning)?;

        while (iter.cur_part_id() as usize) < partitioning.partition_count() {
            let cur_part_id = iter.cur_part_id();
            let mut writer = IpcCompressionWriter::new(RssWriter::new(
                rss_partition_writer.clone(),
                cur_part_id as usize,
            ));

            // write all batches with this part id
            while iter.cur_part_id() == cur_part_id {
                let batch = iter.next_batch();
                writer.write_batch(batch.num_rows(), batch.columns())?;
            }
            writer.finish_current_buf()?;
        }
        jni_call!(BlazeRssPartitionWriterBase(rss_partition_writer.as_obj()).flush() -> ())?;

        log::info!("[partition={partition_id}] all buffered data drained to rss");
        Ok(())
    }

    fn into_sorted_batches(
        mut self,
        partitioning: &Partitioning,
    ) -> Result<PartitionedBatchesIterator> {
        if !self.staging_batches.is_empty() {
            self.flush_staging_batches(partitioning)?;
        }

        let sub_batch_size =
            compute_suggested_batch_size_for_output(self.mem_used(), self.num_rows);

        Ok(PartitionedBatchesIterator {
            batches: self.sorted_batches.clone(),
            cursors: RadixTournamentTree::new(
                self.sorted_parts
                    .into_iter()
                    .enumerate()
                    .map(|(idx, partition_indices)| PartCursor {
                        idx,
                        parts: partition_indices,
                        parts_idx: 0,
                    })
                    .collect(),
                partitioning.partition_count(),
            ),
            num_output_rows: 0,
            num_rows: self.num_rows,
            batch_size: sub_batch_size,
        })
    }

    pub fn mem_used(&self) -> usize {
        self.staging_mem_used + self.sorted_mem_used
    }
}

struct PartitionedBatchesIterator {
    batches: Vec<RecordBatch>,
    cursors: RadixTournamentTree<PartCursor>,
    num_output_rows: usize,
    num_rows: usize,
    batch_size: usize,
}

impl PartitionedBatchesIterator {
    pub fn cur_part_id(&self) -> u32 {
        self.cursors.peek().rdx() as u32
    }

    fn next_batch(&mut self) -> RecordBatch {
        let cur_batch_size = self.batch_size.min(self.num_rows - self.num_output_rows);
        let cur_part_id = self.cur_part_id();
        let mut slices = vec![];
        let mut slices_len = 0;

        // add rows with same parition id under this cursor
        while slices_len < cur_batch_size {
            let mut min_cursor = self.cursors.peek_mut();
            if min_cursor.rdx() as u32 != cur_part_id {
                break;
            }

            let cur_part = min_cursor.parts[min_cursor.parts_idx];
            let cur_slice =
                self.batches[min_cursor.idx].slice(cur_part.start as usize, cur_part.len as usize);
            slices_len += cur_slice.num_rows();
            slices.push(cur_slice);
            min_cursor.parts_idx += 1;
        }
        let output_batch = coalesce_batches_unchecked(self.batches[0].schema(), &slices);
        self.num_output_rows += output_batch.num_rows();
        output_batch
    }
}

struct PartCursor {
    idx: usize,
    parts: Vec<PartitionInBatch>,
    parts_idx: usize,
}

impl KeyForRadixTournamentTree for PartCursor {
    fn rdx(&self) -> usize {
        if self.parts_idx < self.parts.len() {
            return self.parts[self.parts_idx].part_id as usize;
        }
        u32::MAX as usize
    }
}

#[derive(Clone, Copy)]
struct PartitionInBatch {
    part_id: u32,
    start: u32,
    len: u32,
}

fn sort_batches_by_partition_id(
    batches: Vec<RecordBatch>,
    partitioning: &Partitioning,
) -> Result<(Vec<PartitionInBatch>, RecordBatch)> {
    let num_partitions = partitioning.partition_count();
    let schema = batches[0].schema();

    let mut indices = batches // partition_id, batch_idx, row_idx
        .iter()
        .enumerate()
        .flat_map(|(batch_idx, batch)| {
            let hashes = evaluate_hashes(partitioning, batch)
                .expect(&format!("error evaluating hashes with {partitioning}"));
            evaluate_partition_ids(&hashes, partitioning.partition_count())
                .into_iter()
                .enumerate()
                .map(move |(row_idx, part_id)| (part_id, batch_idx as u32, row_idx as u32))
        })
        .collect::<Vec<_>>();

    // sort indices by radix sort
    if num_partitions < 65536 {
        radix_sort_unstable_by_key(&mut indices, |v| v.0 as u16);
    } else {
        radix_sort_unstable_by_key(&mut indices, |v| v.0);
    }

    // get sorted batches
    let (sorted_partition_indices, sorted_row_indices): (Vec<u32>, Vec<_>) = indices
        .into_iter()
        .map(|(part_id, batch_idx, row_idx)| (part_id, (batch_idx as usize, row_idx as usize)))
        .unzip();
    let sorted_batch = interleave_batches(schema, &batches, &sorted_row_indices)?;

    let mut start = 0;
    let partitions = sorted_partition_indices
        .into_iter()
        .chunk_by(|part_id| *part_id)
        .into_iter()
        .map(|(part_id, chunk)| {
            let partition = PartitionInBatch {
                part_id,
                start,
                len: chunk.count() as u32,
            };
            start += partition.len;
            partition
        })
        .collect();

    return Ok((partitions, sorted_batch));
}
