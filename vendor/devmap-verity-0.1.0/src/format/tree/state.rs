// SPDX-License-Identifier: Apache-2.0

use std::io;

use digest::DynDigest;

use super::{Drain, Failure, FlushMode};
use crate::{HashType, Parameters, Salt};

pub(super) struct State {
    hash_type: HashType,
    salt: Salt,
    hasher: Box<dyn DynDigest + Send + Sync>,
    digest: Box<[u8]>,
    maximum_size: u64,
    written: u64,
    data_block: Box<[u8]>,
    data_used: usize,
    levels: Vec<HashLevel>,
    slot_size: usize,
    hashes_per_block: usize,
    tree_size: u64,
    root: Option<Box<[u8]>>,
}

impl State {
    pub(super) fn new(
        parameters: &Parameters,
        hasher: Box<dyn DynDigest + Send + Sync>,
    ) -> io::Result<Self> {
        let digest_size = hasher.output_size();
        let data_block_size = parameters.data.size.bytes().get();
        let hash_block_size = parameters.hash.bytes().get();
        let maximum_size = parameters.data_size()?;
        let hashes_per_block = parameters.hashes_per_block()?;
        let tree_size = parameters.tree_size()?;
        let blocks: Vec<_> = parameters.level_blocks(hashes_per_block).collect();
        let mut offsets = vec![0; blocks.len()];
        let mut offset = 0u64;
        for index in (0..blocks.len()).rev() {
            offsets[index] = offset;
            offset = blocks[index]
                .checked_mul(u64::from(hash_block_size))
                .and_then(|size| offset.checked_add(size))
                .ok_or_else(overflow)?;
        }
        debug_assert_eq!(offset, tree_size);
        let levels = offsets
            .into_iter()
            .map(|offset| HashLevel::new(offset, hash_block_size as usize))
            .collect();
        let slot_size = match parameters.hash_type {
            HashType::ChromeOs => digest_size,
            HashType::Normal => digest_size.next_power_of_two(),
        };

        Ok(Self {
            hash_type: parameters.hash_type,
            salt: parameters.salt,
            hasher,
            digest: vec![0; digest_size].into_boxed_slice(),
            maximum_size,
            written: 0,
            data_block: vec![0; data_block_size as usize].into_boxed_slice(),
            data_used: 0,
            levels,
            slot_size,
            hashes_per_block,
            tree_size,
            root: None,
        })
    }

    pub(super) fn accept(&mut self, input: &[u8]) -> Result<usize, Failure> {
        if input.is_empty() {
            return Ok(0);
        }
        let input_len = u64::try_from(input.len()).map_err(|_| {
            Failure::Recoverable(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input length does not fit u64",
            ))
        })?;
        let next = self.written.checked_add(input_len).ok_or_else(|| {
            Failure::Recoverable(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input length overflow",
            ))
        })?;
        if next > self.maximum_size {
            return Err(Failure::Recoverable(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input exceeds declared data block count",
            )));
        }

        let block_size = self.data_block.len();
        if self.data_used == 0 && input.len() >= block_size {
            self.process_block(&input[..block_size])
                .map_err(Failure::Fatal)?;
            self.written += block_size as u64;
            return Ok(block_size);
        }

        let copied = (block_size - self.data_used).min(input.len());
        self.data_block[self.data_used..self.data_used + copied].copy_from_slice(&input[..copied]);
        self.data_used += copied;
        self.written += copied as u64;
        if self.data_used == block_size {
            self.process_data_block().map_err(Failure::Fatal)?;
        }
        Ok(copied)
    }

    pub(super) const fn begin_flush(&self) -> FlushMode {
        if self.reached_end() {
            FlushMode::Final
        } else {
            FlushMode::Intermediate
        }
    }

    pub(super) const fn reached_end(&self) -> bool {
        self.written == self.maximum_size
    }

    pub(super) fn take_pending(&mut self, mode: Drain) -> Option<PendingBlock> {
        if self
            .levels
            .iter()
            .any(|level| matches!(level, HashLevel::InFlight))
        {
            return None;
        }

        let level = self.levels.iter().position(|level| match level {
            HashLevel::Filling(buffer) => match mode {
                Drain::Full => buffer.is_full(self.hashes_per_block),
                Drain::All => buffer.slots_used != 0,
            },
            HashLevel::InFlight => false,
        })?;
        let HashLevel::Filling(buffer) =
            std::mem::replace(&mut self.levels[level], HashLevel::InFlight)
        else {
            return None;
        };

        Some(PendingBlock {
            level: LevelId(level),
            offset: TreeOffset(buffer.offset),
            bytes: buffer.bytes,
        })
    }

    pub(super) fn commit(&mut self, mut pending: PendingBlock) -> io::Result<()> {
        let level = pending.level.0;
        if !matches!(self.levels.get(level), Some(HashLevel::InFlight)) {
            return Err(io::Error::other("invalid pending hash-tree block"));
        }

        self.hash_type.digest(
            self.hasher.as_mut(),
            self.salt.as_ref(),
            &pending.bytes,
            &mut self.digest,
        )?;

        let next_offset = pending
            .offset
            .0
            .checked_add(pending.bytes.len() as u64)
            .ok_or_else(|| io::Error::other("hash-tree level offset overflows"))?;
        pending.bytes.fill(0);
        self.levels[level] = HashLevel::Filling(LevelBuffer {
            offset: next_offset,
            bytes: pending.bytes,
            slots_used: 0,
        });

        if level + 1 == self.levels.len() {
            self.root = Some(std::mem::take(&mut self.digest));
        } else {
            self.levels[level + 1].push(&self.digest, self.slot_size)?;
        }
        Ok(())
    }

    pub(super) fn take_digest(&mut self) -> Option<Box<[u8]>> {
        self.root.take()
    }

    pub(super) const fn tree_size(&self) -> u64 {
        self.tree_size
    }

    fn process_data_block(&mut self) -> io::Result<()> {
        self.hash_type.digest(
            self.hasher.as_mut(),
            self.salt.as_ref(),
            &self.data_block,
            &mut self.digest,
        )?;
        if self.levels.is_empty() {
            self.root = Some(std::mem::take(&mut self.digest));
        } else {
            self.levels[0].push(&self.digest, self.slot_size)?;
        }
        self.data_block.fill(0);
        self.data_used = 0;
        Ok(())
    }

    fn process_block(&mut self, block: &[u8]) -> io::Result<()> {
        self.hash_type.digest(
            self.hasher.as_mut(),
            self.salt.as_ref(),
            block,
            &mut self.digest,
        )?;
        if self.levels.is_empty() {
            self.root = Some(std::mem::take(&mut self.digest));
        } else {
            self.levels[0].push(&self.digest, self.slot_size)?;
        }
        Ok(())
    }
}

fn overflow() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "verity layout overflows")
}

enum HashLevel {
    Filling(LevelBuffer),
    InFlight,
}

impl HashLevel {
    fn new(offset: u64, block_size: usize) -> Self {
        Self::Filling(LevelBuffer {
            offset,
            bytes: vec![0; block_size].into_boxed_slice(),
            slots_used: 0,
        })
    }

    fn push(&mut self, digest: &[u8], slot_size: usize) -> io::Result<()> {
        let Self::Filling(buffer) = self else {
            return Err(io::Error::other("hash-tree level is being written"));
        };
        let start = buffer
            .slots_used
            .checked_mul(slot_size)
            .ok_or_else(|| io::Error::other("hash-tree slot offset overflows"))?;
        let end = start
            .checked_add(digest.len())
            .ok_or_else(|| io::Error::other("hash-tree digest offset overflows"))?;
        let destination = buffer
            .bytes
            .get_mut(start..end)
            .ok_or_else(|| io::Error::other("hash-tree level cannot hold another digest"))?;
        destination.copy_from_slice(digest);
        buffer.slots_used += 1;
        Ok(())
    }
}

struct LevelBuffer {
    offset: u64,
    bytes: Box<[u8]>,
    slots_used: usize,
}

impl LevelBuffer {
    const fn is_full(&self, hashes_per_block: usize) -> bool {
        self.slots_used == hashes_per_block
    }
}

#[derive(Clone, Copy)]
struct LevelId(usize);

#[derive(Clone, Copy)]
struct TreeOffset(u64);

pub(super) struct PendingBlock {
    level: LevelId,
    offset: TreeOffset,
    bytes: Box<[u8]>,
}

impl PendingBlock {
    pub(super) const fn offset(&self) -> u64 {
        self.offset.0
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
