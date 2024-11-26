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
    any::Any,
    fmt::{Debug, Formatter},
    hash::BuildHasher,
    io::{Cursor, Read, Write},
    marker::PhantomData,
    sync::Arc,
};

use arrow::{array::*, datatypes::*};
use datafusion::{
    common::{Result, ScalarValue},
    physical_expr::PhysicalExpr,
};
use datafusion_ext_commons::{
    downcast_any,
    io::{read_bytes_slice, read_len, read_scalar, write_len, write_scalar},
};
use hashbrown::raw::RawTable;
use smallvec::SmallVec;

use crate::{
    agg::{
        acc::{AccColumn, AccColumnRef},
        agg::{Agg, IdxSelection},
    },
    idx_for, idx_for_zipped,
    memmgr::spill::{SpillCompressedReader, SpillCompressedWriter},
};

pub type AggCollectSet = AggGenericCollect<AccSetColumn>;
pub type AggCollectList = AggGenericCollect<AccListColumn>;

pub struct AggGenericCollect<C: AccCollectionColumn> {
    child: Arc<dyn PhysicalExpr>,
    data_type: DataType,
    arg_type: DataType,
    _phantom: PhantomData<C>,
}

impl<C: AccCollectionColumn> AggGenericCollect<C> {
    pub fn try_new(
        child: Arc<dyn PhysicalExpr>,
        data_type: DataType,
        arg_type: DataType,
    ) -> Result<Self> {
        Ok(Self {
            child,
            data_type,
            arg_type,
            _phantom: Default::default(),
        })
    }

    pub fn arg_type(&self) -> &DataType {
        &self.arg_type
    }
}

impl<C: AccCollectionColumn> Debug for AggGenericCollect<C> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Collect({:?})", self.child)
    }
}

impl<C: AccCollectionColumn> Agg for AggGenericCollect<C> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn exprs(&self) -> Vec<Arc<dyn PhysicalExpr>> {
        vec![self.child.clone()]
    }

    fn with_new_exprs(&self, exprs: Vec<Arc<dyn PhysicalExpr>>) -> Result<Arc<dyn Agg>> {
        Ok(Arc::new(Self::try_new(
            exprs[0].clone(),
            self.data_type.clone(),
            self.arg_type.clone(),
        )?))
    }

    fn data_type(&self) -> &DataType {
        &self.data_type
    }

    fn nullable(&self) -> bool {
        false
    }

    fn create_acc_column(&self, num_rows: usize) -> AccColumnRef {
        let mut col = Box::new(C::empty(self.arg_type.clone()));
        col.resize(num_rows);
        col
    }

    fn partial_update(
        &self,
        accs: &mut AccColumnRef,
        acc_idx: IdxSelection<'_>,
        partial_args: &[ArrayRef],
        partial_arg_idx: IdxSelection<'_>,
    ) -> Result<()> {
        let accs = downcast_any!(accs, mut C).unwrap();
        idx_for_zipped! {
            ((acc_idx, partial_arg_idx) in (acc_idx, partial_arg_idx)) => {
                let scalar = ScalarValue::try_from_array(&partial_args[0], partial_arg_idx)?;
                if !scalar.is_null() {
                    accs.append_item(acc_idx, &scalar);
                }
            }
        }
        Ok(())
    }

    fn partial_merge(
        &self,
        accs: &mut AccColumnRef,
        acc_idx: IdxSelection<'_>,
        merging_accs: &mut AccColumnRef,
        merging_acc_idx: IdxSelection<'_>,
    ) -> Result<()> {
        let accs = downcast_any!(accs, mut C).unwrap();
        let merging_accs = downcast_any!(merging_accs, mut C).unwrap();

        idx_for_zipped! {
            ((acc_idx, merging_acc_idx) in (acc_idx, merging_acc_idx)) => {
                accs.merge_items(acc_idx, merging_accs, merging_acc_idx);
            }
        }
        Ok(())
    }

    fn final_merge(&self, accs: &mut AccColumnRef, acc_idx: IdxSelection<'_>) -> Result<ArrayRef> {
        let accs = downcast_any!(accs, mut C).unwrap();
        let mut list = Vec::with_capacity(accs.num_records());

        idx_for! {
            (acc_idx in acc_idx) => {
                list.push(ScalarValue::List(ScalarValue::new_list(
                    &accs.take_values(acc_idx, self.arg_type.clone()),
                    &self.arg_type,
                    true,
                )));
            }
        }
        ScalarValue::iter_to_array(list)
    }
}

pub trait AccCollectionColumn: AccColumn + Send + Sync + 'static {
    fn empty(dt: DataType) -> Self;
    fn append_item(&mut self, idx: usize, value: &ScalarValue);
    fn merge_items(&mut self, idx: usize, other: &mut Self, other_idx: usize);
    fn save_raw(&self, idx: usize, w: &mut impl Write) -> Result<()>;
    fn load_raw(&mut self, idx: usize, r: &mut impl Read) -> Result<()>;
    fn take_values(&mut self, idx: usize, dt: DataType) -> Vec<ScalarValue>;

    fn freeze_to_rows(&self, idx: IdxSelection<'_>, array: &mut [Vec<u8>]) -> Result<()> {
        let mut array_idx = 0;

        idx_for! {
            (idx in idx) => {
                self.save_raw(idx, &mut array[array_idx])?;
                array_idx += 1;
            }
        }
        Ok(())
    }

    fn unfreeze_from_rows(&mut self, array: &[&[u8]], offsets: &mut [usize]) -> Result<()> {
        let mut idx = self.num_records();
        self.resize(idx + array.len());

        for (raw, offset) in array.iter().zip(offsets) {
            let mut cursor = Cursor::new(raw);
            cursor.set_position(*offset as u64);
            self.load_raw(idx, &mut cursor)?;
            *offset = cursor.position() as usize;
            idx += 1;
        }
        Ok(())
    }

    fn spill(&self, idx: IdxSelection<'_>, w: &mut SpillCompressedWriter) -> Result<()> {
        idx_for! {
            (idx in idx) => {
                self.save_raw(idx, w)?;
            }
        }
        Ok(())
    }

    fn unspill(&mut self, num_rows: usize, r: &mut SpillCompressedReader) -> Result<()> {
        let idx = self.num_records();
        self.resize(idx + num_rows);

        while idx < self.num_records() {
            self.load_raw(idx, r)?;
        }
        Ok(())
    }
}

pub struct AccSetColumn {
    set: Vec<AccSet>,
    dt: DataType,
    mem_used: usize,
}

impl AccCollectionColumn for AccSetColumn {
    fn empty(dt: DataType) -> Self {
        Self {
            set: vec![],
            dt,
            mem_used: 0,
        }
    }

    fn append_item(&mut self, idx: usize, value: &ScalarValue) {
        let old_mem_size = self.set[idx].mem_size();
        self.set[idx].append(value, false);
        self.mem_used += self.set[idx].mem_size() - old_mem_size;
    }

    fn merge_items(&mut self, idx: usize, other: &mut Self, other_idx: usize) {
        let self_value_mem_size = self.set[idx].mem_size();
        let other_value_mem_size = other.set[other_idx].mem_size();
        self.set[idx].merge(&mut other.set[other_idx]);
        self.mem_used += self.set[idx].mem_size() - self_value_mem_size;
        other.mem_used -= other_value_mem_size;
    }

    fn save_raw(&self, idx: usize, w: &mut impl Write) -> Result<()> {
        write_len(self.set[idx].list.raw.len(), w)?;
        w.write_all(&self.set[idx].list.raw)?;
        Ok(())
    }

    fn load_raw(&mut self, idx: usize, r: &mut impl Read) -> Result<()> {
        self.mem_used -= self.set[idx].mem_size();
        self.set[idx] = AccSet::default();

        let len = read_len(r)?;
        let mut cursor = Cursor::new(read_bytes_slice(r, len)?);
        while cursor.position() < len as u64 {
            let scalar = read_scalar(&mut cursor, &self.dt, false)?;
            self.append_item(idx, &scalar);
        }
        self.mem_used += self.set[idx].mem_size();
        Ok(())
    }

    fn take_values(&mut self, idx: usize, dt: DataType) -> Vec<ScalarValue> {
        self.mem_used -= self.set[idx].mem_size();
        std::mem::take(&mut self.set[idx])
            .into_values(dt, false)
            .collect()
    }
}

impl AccColumn for AccSetColumn {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn resize(&mut self, len: usize) {
        if len < self.set.len() {
            for idx in len..self.set.len() {
                self.mem_used -= self.set[idx].mem_size();
                self.set[idx] = AccSet::default();
            }
        }
        self.set.resize_with(len, || AccSet::default());
    }

    fn shrink_to_fit(&mut self) {
        self.set.shrink_to_fit();
    }

    fn num_records(&self) -> usize {
        self.set.len()
    }

    fn mem_used(&self) -> usize {
        self.mem_used + self.set.capacity() * size_of::<AccSet>()
    }

    fn freeze_to_rows(&self, idx: IdxSelection<'_>, array: &mut [Vec<u8>]) -> Result<()> {
        AccCollectionColumn::freeze_to_rows(self, idx, array)
    }

    fn unfreeze_from_rows(&mut self, array: &[&[u8]], offsets: &mut [usize]) -> Result<()> {
        AccCollectionColumn::unfreeze_from_rows(self, array, offsets)
    }

    fn spill(&self, idx: IdxSelection<'_>, w: &mut SpillCompressedWriter) -> Result<()> {
        AccCollectionColumn::spill(self, idx, w)
    }

    fn unspill(&mut self, num_rows: usize, r: &mut SpillCompressedReader) -> Result<()> {
        AccCollectionColumn::unspill(self, num_rows, r)
    }
}

pub struct AccListColumn {
    list: Vec<AccList>,
    mem_used: usize,
}

impl AccCollectionColumn for AccListColumn {
    fn empty(_dt: DataType) -> Self {
        Self {
            list: vec![],
            mem_used: 0,
        }
    }

    fn append_item(&mut self, idx: usize, value: &ScalarValue) {
        let old_mem_size = self.list[idx].mem_size();
        self.list[idx].append(value, false);
        self.mem_used += self.list[idx].mem_size() - old_mem_size;
    }

    fn merge_items(&mut self, idx: usize, other: &mut Self, other_idx: usize) {
        let self_value_mem_size = self.list[idx].mem_size();
        let other_value_mem_size = other.list[other_idx].mem_size();
        self.list[idx].merge(&mut other.list[other_idx]);
        self.mem_used += self.list[idx].mem_size() - self_value_mem_size;
        other.mem_used -= other_value_mem_size;
    }

    fn save_raw(&self, idx: usize, w: &mut impl Write) -> Result<()> {
        write_len(self.list[idx].raw.len(), w)?;
        w.write_all(&self.list[idx].raw)?;
        Ok(())
    }

    fn load_raw(&mut self, idx: usize, r: &mut impl Read) -> Result<()> {
        self.mem_used -= self.list[idx].mem_size();
        self.list[idx] = AccList::default();

        let len = read_len(r)?;
        self.list[idx].raw = read_bytes_slice(r, len)?.into();
        self.mem_used += self.list[idx].mem_size();
        Ok(())
    }

    fn take_values(&mut self, idx: usize, dt: DataType) -> Vec<ScalarValue> {
        self.mem_used -= self.list[idx].mem_size();
        std::mem::take(&mut self.list[idx])
            .into_values(dt, false)
            .collect()
    }
}

impl AccColumn for AccListColumn {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn resize(&mut self, len: usize) {
        if len < self.list.len() {
            for idx in len..self.list.len() {
                self.mem_used -= self.list[idx].mem_size();
                self.list[idx] = AccList::default();
            }
        }
        self.list.resize_with(len, || AccList::default());
    }

    fn shrink_to_fit(&mut self) {
        self.list.shrink_to_fit();
    }

    fn num_records(&self) -> usize {
        self.list.len()
    }

    fn mem_used(&self) -> usize {
        self.mem_used + self.list.capacity() * size_of::<AccList>()
    }

    fn freeze_to_rows(&self, idx: IdxSelection<'_>, array: &mut [Vec<u8>]) -> Result<()> {
        AccCollectionColumn::freeze_to_rows(self, idx, array)
    }

    fn unfreeze_from_rows(&mut self, array: &[&[u8]], offsets: &mut [usize]) -> Result<()> {
        AccCollectionColumn::unfreeze_from_rows(self, array, offsets)
    }

    fn spill(&self, idx: IdxSelection<'_>, w: &mut SpillCompressedWriter) -> Result<()> {
        AccCollectionColumn::spill(self, idx, w)
    }

    fn unspill(&mut self, num_rows: usize, r: &mut SpillCompressedReader) -> Result<()> {
        AccCollectionColumn::unspill(self, num_rows, r)
    }
}

#[derive(Clone, Default)]
struct AccList {
    raw: Vec<u8>,
}

impl AccList {
    pub fn from_raw(raw: Vec<u8>) -> Self {
        Self { raw }
    }

    pub fn mem_size(&self) -> usize {
        self.raw.capacity()
    }

    pub fn append(&mut self, value: &ScalarValue, nullable: bool) {
        write_scalar(&value, nullable, &mut self.raw).unwrap();
    }

    pub fn merge(&mut self, other: &mut Self) {
        self.raw.extend(std::mem::take(&mut other.raw));
    }

    pub fn into_values(self, dt: DataType, nullable: bool) -> impl Iterator<Item = ScalarValue> {
        struct ValuesIterator(Cursor<Vec<u8>>, DataType, bool);
        impl Iterator for ValuesIterator {
            type Item = ScalarValue;

            fn next(&mut self) -> Option<Self::Item> {
                if self.0.position() < self.0.get_ref().len() as u64 {
                    return Some(read_scalar(&mut self.0, &self.1, self.2).unwrap());
                }
                None
            }
        }
        ValuesIterator(Cursor::new(self.raw), dt, nullable)
    }

    fn ref_raw(&self, pos_len: (u32, u32)) -> &[u8] {
        &self.raw[pos_len.0 as usize..][..pos_len.1 as usize]
    }
}

#[derive(Clone, Default)]
struct AccSet {
    list: AccList,
    set: InternalSet,
}

#[derive(Clone)]
enum InternalSet {
    Small(SmallVec<(u32, u32), 4>),
    Huge(RawTable<(u32, u32)>),
}

impl Default for InternalSet {
    fn default() -> Self {
        Self::Small(SmallVec::new())
    }
}

impl InternalSet {
    fn len(&self) -> usize {
        match self {
            InternalSet::Small(s) => s.len(),
            InternalSet::Huge(s) => s.len(),
        }
    }

    fn into_iter(self) -> impl Iterator<Item = (u32, u32)> {
        let iter: Box<dyn Iterator<Item = (u32, u32)>> = match self {
            InternalSet::Small(s) => Box::new(s.into_iter()),
            InternalSet::Huge(s) => Box::new(s.into_iter()),
        };
        iter
    }

    fn convert_to_huge_if_needed(&mut self, list: &mut AccList) {
        if let Self::Small(s) = self {
            let mut huge = RawTable::default();

            for &mut pos_len in s {
                let raw = list.ref_raw(pos_len);
                let hash = acc_hash(raw);
                huge.insert(hash, pos_len, |&pos_len| acc_hash(list.ref_raw(pos_len)));
            }
            *self = Self::Huge(huge);
        }
    }
}

impl AccSet {
    pub fn mem_size(&self) -> usize {
        // mem size of internal set is estimated for faster computation
        self.list.mem_size() + self.set.len() * size_of::<(u32, u32)>()
    }

    pub fn append(&mut self, value: &ScalarValue, nullable: bool) {
        let old_raw_len = self.list.raw.len();
        write_scalar(value, nullable, &mut self.list.raw).unwrap();
        self.append_raw_inline(old_raw_len);
    }

    pub fn merge(&mut self, other: &mut Self) {
        if self.set.len() < other.set.len() {
            // ensure the probed set is smaller
            std::mem::swap(self, other);
        }
        for pos_len in std::mem::take(&mut other.set).into_iter() {
            self.append_raw(other.list.ref_raw(pos_len));
        }
    }

    pub fn into_values(self, dt: DataType, nullable: bool) -> impl Iterator<Item = ScalarValue> {
        self.list.into_values(dt, nullable)
    }

    fn append_raw(&mut self, raw: &[u8]) {
        let new_len = raw.len();
        let new_pos_len = (self.list.raw.len() as u32, new_len as u32);

        match &mut self.set {
            InternalSet::Small(s) => {
                let mut found = false;
                for &mut pos_len in &mut *s {
                    if self.list.ref_raw(pos_len) == raw {
                        found = true;
                        break;
                    }
                }
                if !found {
                    s.push(new_pos_len);
                    self.list.raw.extend(raw);
                    self.set.convert_to_huge_if_needed(&mut self.list);
                }
            }
            InternalSet::Huge(s) => {
                let hash = acc_hash(raw);
                match s.find_or_find_insert_slot(
                    hash,
                    |&pos_len| new_len == pos_len.1 as usize && raw == self.list.ref_raw(pos_len),
                    |&pos_len| acc_hash(self.list.ref_raw(pos_len)),
                ) {
                    Ok(_found) => {}
                    Err(slot) => {
                        unsafe {
                            // safety: call unsafe `insert_in_slot` method
                            self.list.raw.extend(raw);
                            s.insert_in_slot(hash, slot, new_pos_len);
                        }
                    }
                }
            }
        }
    }

    fn append_raw_inline(&mut self, raw_start: usize) {
        let new_len = self.list.raw.len() - raw_start;
        let new_pos_len = (raw_start as u32, new_len as u32);
        let mut inserted = true;

        match &mut self.set {
            InternalSet::Small(s) => {
                for &mut pos_len in &mut *s {
                    if self.list.ref_raw(pos_len) == self.list.ref_raw(new_pos_len) {
                        inserted = false;
                        break;
                    }
                }
                if inserted {
                    s.push(new_pos_len);
                    self.set.convert_to_huge_if_needed(&mut self.list);
                }
            }
            InternalSet::Huge(s) => {
                let new_value = self.list.ref_raw(new_pos_len);
                let hash = acc_hash(new_value);
                match s.find_or_find_insert_slot(
                    hash,
                    |&pos_len| {
                        new_len == pos_len.1 as usize && new_value == self.list.ref_raw(pos_len)
                    },
                    |&pos_len| acc_hash(self.list.ref_raw(pos_len)),
                ) {
                    Ok(_found) => {
                        inserted = false;
                    }
                    Err(slot) => {
                        unsafe {
                            // safety: call unsafe `insert_in_slot` method
                            s.insert_in_slot(hash, slot, new_pos_len);
                        }
                    }
                }
            }
        }

        // remove the value from list if not inserted
        if !inserted {
            self.list.raw.truncate(raw_start);
        }
    }
}

#[inline]
fn acc_hash(value: impl AsRef<[u8]>) -> u64 {
    const ACC_HASH_SEED: u32 = 0x7BCB48DA;
    const HASHER: foldhash::fast::FixedState =
        foldhash::fast::FixedState::with_seed(ACC_HASH_SEED as u64);
    HASHER.hash_one(value.as_ref())
}