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

use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering::Relaxed},
        Arc,
    },
};

use arrow::{
    array::{ArrayRef, BooleanArray, RecordBatch},
    buffer::NullBuffer,
};
use async_trait::async_trait;
use bitvec::{bitvec, prelude::BitVec};
use datafusion::{common::Result, physical_plan::metrics::Time};
use hashbrown::HashSet;

use crate::{
    broadcast_join_exec::Joiner,
    common::{batch_selection::take_cols, output::WrappedRecordBatchSender},
    joins::{
        bhj::{
            make_eq_comparator_multiple_arrays,
            semi_join::{
                ProbeSide::{L, R},
                SemiMode::{Anti, Existence, Semi},
            },
            ProbeSide,
        },
        join_hash_map::{join_create_hashes, JoinHashMap},
        JoinParams,
    },
};

#[derive(std::marker::ConstParamTy, Clone, Copy, PartialEq, Eq)]
pub enum SemiMode {
    Semi,
    Anti,
    Existence,
}

#[derive(std::marker::ConstParamTy, Clone, Copy, PartialEq, Eq)]
pub struct JoinerParams {
    probe_side: ProbeSide,
    probe_is_join_side: bool,
    mode: SemiMode,
}

impl JoinerParams {
    const fn new(probe_side: ProbeSide, probe_is_join_side: bool, mode: SemiMode) -> Self {
        Self {
            probe_side,
            probe_is_join_side,
            mode,
        }
    }
}

const LEFT_PROBED_LEFT_SEMI: JoinerParams = JoinerParams::new(L, true, Semi);
const LEFT_PROBED_LEFT_ANTI: JoinerParams = JoinerParams::new(L, true, Anti);
const LEFT_PROBED_RIGHT_SEMI: JoinerParams = JoinerParams::new(L, false, Semi);
const LEFT_PROBED_RIGHT_ANTI: JoinerParams = JoinerParams::new(L, false, Anti);
const LEFT_PROBED_EXISTENCE: JoinerParams = JoinerParams::new(L, true, Existence);
const RIGHT_PROBED_LEFT_SEMI: JoinerParams = JoinerParams::new(R, false, Semi);
const RIGHT_PROBED_LEFT_ANTI: JoinerParams = JoinerParams::new(R, false, Anti);
const RIGHT_PROBED_RIGHT_SEMI: JoinerParams = JoinerParams::new(R, true, Semi);
const RIGHT_PROBED_RIGHT_ANTI: JoinerParams = JoinerParams::new(R, true, Anti);
const RIGHT_PROBED_EXISTENCE: JoinerParams = JoinerParams::new(R, false, Existence);

pub type LProbedLeftSemiJoiner = SemiJoiner<LEFT_PROBED_LEFT_SEMI>;
pub type LProbedLeftAntiJoiner = SemiJoiner<LEFT_PROBED_LEFT_ANTI>;
pub type LProbedRightSemiJoiner = SemiJoiner<LEFT_PROBED_RIGHT_SEMI>;
pub type LProbedRightAntiJoiner = SemiJoiner<LEFT_PROBED_RIGHT_ANTI>;
pub type LProbedExistenceJoiner = SemiJoiner<LEFT_PROBED_EXISTENCE>;
pub type RProbedLeftSemiJoiner = SemiJoiner<RIGHT_PROBED_LEFT_SEMI>;
pub type RProbedLeftAntiJoiner = SemiJoiner<RIGHT_PROBED_LEFT_ANTI>;
pub type RProbedRightSemiJoiner = SemiJoiner<RIGHT_PROBED_RIGHT_SEMI>;
pub type RProbedRightAntiJoiner = SemiJoiner<RIGHT_PROBED_RIGHT_ANTI>;
pub type RProbedExistenceJoiner = SemiJoiner<RIGHT_PROBED_EXISTENCE>;

pub struct SemiJoiner<const P: JoinerParams> {
    join_params: JoinParams,
    output_sender: Arc<WrappedRecordBatchSender>,
    map_joined: BitVec,
    hash_skippable: HashSet<i32>,
    map: Arc<JoinHashMap>,
    send_output_time: Time,
    output_rows: AtomicUsize,
}

impl<const P: JoinerParams> SemiJoiner<P> {
    pub fn new(
        join_params: JoinParams,
        map: Arc<JoinHashMap>,
        output_sender: Arc<WrappedRecordBatchSender>,
    ) -> Self {
        let map_joined = bitvec![0; map.data_batch().num_rows()];
        Self {
            join_params,
            output_sender,
            map,
            map_joined,
            hash_skippable: HashSet::new(),
            send_output_time: Time::new(),
            output_rows: AtomicUsize::new(0),
        }
    }

    fn create_probed_key_columns(&self, probed_batch: &RecordBatch) -> Result<Vec<ArrayRef>> {
        let probed_key_exprs = match P.probe_side {
            L => &self.join_params.left_keys,
            R => &self.join_params.right_keys,
        };
        let probed_key_columns: Vec<ArrayRef> = probed_key_exprs
            .iter()
            .map(|expr| {
                Ok(expr
                    .evaluate(probed_batch)?
                    .into_array(probed_batch.num_rows())?)
            })
            .collect::<Result<_>>()?;
        Ok(probed_key_columns)
    }

    async fn flush(&self, cols: Vec<ArrayRef>) -> Result<()> {
        let output_batch = RecordBatch::try_new(self.join_params.output_schema.clone(), cols)?;
        self.output_rows.fetch_add(output_batch.num_rows(), Relaxed);

        let timer = self.send_output_time.timer();
        self.output_sender.send(Ok(output_batch), None).await;
        drop(timer);
        Ok(())
    }
}

#[async_trait]
impl<const P: JoinerParams> Joiner for SemiJoiner<P> {
    async fn join(mut self: Pin<&mut Self>, probed_batch: RecordBatch) -> Result<()> {
        let mut hash_joined_probe_indices: Vec<u32> = vec![];
        let mut hash_joined_build_indices: Vec<u32> = vec![];
        let mut probed_joined = bitvec![0; probed_batch.num_rows()];
        let map_joined = unsafe {
            // safety: ignore r/w conflicts with self.map
            std::mem::transmute::<_, &mut BitVec>(&mut self.map_joined)
        };

        let probed_key_columns = self.create_probed_key_columns(&probed_batch)?;
        let probed_hashes = join_create_hashes(probed_batch.num_rows(), &probed_key_columns)?;
        let probed_valids = probed_key_columns
            .iter()
            .map(|col| col.nulls().cloned())
            .reduce(|nb1, nb2| NullBuffer::union(nb1.as_ref(), nb2.as_ref()))
            .flatten();

        let eq = make_eq_comparator_multiple_arrays(&probed_key_columns, self.map.key_columns())?;

        // join by hash code
        for (row_idx, &hash) in probed_hashes.iter().enumerate() {
            // nulls may not be joined
            if probed_valids
                .as_ref()
                .map(|nb| nb.is_null(row_idx))
                .unwrap_or(false)
            {
                continue;
            }

            // map is join side -- skip hashes those may not be joined
            if !P.probe_is_join_side && self.hash_skippable.contains(&hash) {
                continue;
            }

            let mut maybe_joined = false;
            if let Some(entries) = self.map.entry_indices(hash) {
                for map_idx in entries {
                    // join only once if map side is the join side
                    if !P.probe_is_join_side && map_joined[map_idx as usize] {
                        continue;
                    }
                    if eq(row_idx, map_idx as usize) {
                        hash_joined_probe_indices.push(row_idx as u32);
                        hash_joined_build_indices.push(map_idx);
                        if P.probe_is_join_side {
                            probed_joined.set(row_idx, true);
                        } else {
                            map_joined.set(map_idx as usize, true);
                        }
                    }
                    maybe_joined = true;
                }
            }

            // map is join side -- mark the hash as skippable
            if !P.probe_is_join_side && !maybe_joined {
                self.hash_skippable.insert(hash);
            }
        }

        if P.probe_is_join_side {
            let pprojected = match P.probe_side {
                L => self
                    .join_params
                    .projection
                    .project_left(probed_batch.columns()),
                R => self
                    .join_params
                    .projection
                    .project_right(probed_batch.columns()),
            };
            let pcols = match P.mode {
                Semi | Anti => {
                    let probed_indices = probed_joined
                        .into_iter()
                        .enumerate()
                        .filter(|(_, joined)| (P.mode == Semi) ^ !joined)
                        .map(|(idx, _)| idx as u32)
                        .collect::<Vec<_>>();
                    take_cols(&pprojected, probed_indices)?
                }
                Existence => {
                    let exists_col = Arc::new(BooleanArray::from(
                        probed_joined.into_iter().collect::<Vec<_>>(),
                    ));
                    [pprojected, vec![exists_col]].concat()
                }
            };
            self.as_mut().flush(pcols).await?;
        }
        Ok(())
    }

    async fn finish(mut self: Pin<&mut Self>) -> Result<()> {
        if !P.probe_is_join_side {
            let mprojected = match P.probe_side {
                L => self
                    .join_params
                    .projection
                    .project_right(self.map.data_batch().columns()),
                R => self
                    .join_params
                    .projection
                    .project_left(self.map.data_batch().columns()),
            };
            let map_joined = std::mem::take(&mut self.map_joined);
            let pcols = match P.mode {
                Semi | Anti => {
                    let map_indices = map_joined
                        .into_iter()
                        .enumerate()
                        .filter(|(_, joined)| (P.mode == Semi) ^ !joined)
                        .map(|(idx, _)| idx as u32)
                        .collect::<Vec<_>>();
                    take_cols(&mprojected, map_indices)?
                }
                Existence => {
                    let exists_col = Arc::new(BooleanArray::from(
                        map_joined.into_iter().collect::<Vec<_>>(),
                    ));
                    [mprojected, vec![exists_col]].concat()
                }
            };
            self.as_mut().flush(pcols).await?;
        }
        Ok(())
    }

    fn can_early_stop(&self) -> bool {
        if !P.probe_is_join_side && self.map_joined.all() {
            // semi join: map is join side and all items are joined
            return true;
        }
        false
    }

    fn total_send_output_time(&self) -> usize {
        self.send_output_time.value()
    }

    fn num_output_rows(&self) -> usize {
        self.output_rows.load(Relaxed)
    }
}
