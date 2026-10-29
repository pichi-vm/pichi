// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use carapace::{Assemble, Chain, FromGpt};
use devmap_core::{BlockSize, Geometry, Span};
use devmap_linux::records::Records;
use devmap_linux::{
    targets::{Target, Zero},
    Control, DevId, Table,
};
use devmap_snapshot::{Discard, Parameters, Persistence, Persistent, Snapshot};
use devmap_verity::{Algorithm, Format, HashType, Header, Salt};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    num::NonZero,
    os::unix::fs::FileExt,
    thread,
    time::{Duration, Instant},
};

const MIB: u64 = 1024 * 1024;
const DATA_SIZE: u64 = 4 * MIB;
const SECTORS: u64 = 64 * 1024 * MIB / 512;
const BLOCK: u64 = 4096;
const WRITES: usize = 32;
pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// Own only mappings created by this fixture. Never remove somebody else's devices.
struct Mapping {
    control: Control,
    id: DevId,
    removed: bool,
}
impl Mapping {
    fn new<T: Target>(control: &Control, name: &str, target: &T) -> Result<Self> {
        let id = control.create(name)?;
        let mapping = Self {
            control: control.clone(),
            id,
            removed: false,
        };
        let table = Table::<4096>::default().add(
            Span::new(0, SECTORS),
            target,
            control.version(T::NAME)?,
        )?;
        control.load(id, table, false)?;
        control.suspend(id, false)?;
        Ok(mapping)
    }
    fn open(&self) -> io::Result<File> {
        File::options()
            .read(true)
            .write(true)
            .open(format!("/dev/dm-{}", self.id.minor()))
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        if let Err(error) = self.control.remove(self.id, true) {
            eprintln!("fixture cleanup {}: {error}", self.id);
        }
    }
}

fn attach(file: &File, partscan: bool) -> Result<lodown::Device> {
    Ok(lodown::Control::open()?.attach(
        file,
        0,
        lodown::Configurable {
            read_only: partscan,
            writable: lodown::Writable {
                autoclear: true,
                partscan,
                ..Default::default()
            },
            ..Default::default()
        },
    )?)
}

/// Refuse to benchmark reuse or interfere with another active consumer.
pub(crate) fn wait_empty(control: &Control) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut devices = control.devices::<65536>()?;
        let mut present = false;
        while let Some(device) = devices.next() {
            let (name, _) = device?;
            present |= name.starts_with("CARAPACE-");
        }
        if !present {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("CARAPACE mappings still present; run on an otherwise idle host".into());
        }
        thread::sleep(Duration::from_millis(1));
    }
}

pub(crate) struct Fixture {
    pub(crate) root: Box<[u8]>,
    // Close loop device before deleting its backing storage.
    _device: lodown::Device,
    _image: File,
}
impl Fixture {
    pub(crate) fn new(control: &Control, depth: usize) -> Result<Self> {
        wait_empty(control)?;
        let mut image = tempfile::tempfile()?;
        let size = (depth as u64 * 5 + 2) * MIB;
        image.set_len(size)?;
        gpt::mbr::ProtectiveMBR::with_lb_size((size / 512 - 1) as u32)
            .overwrite_lba0(&mut image)?;
        let mut disk = gpt::GptConfig::new()
            .writable(true)
            .create_from_device(image.try_clone()?, None)?;
        let mut root: Box<[u8]> = vec![0; 32].into_boxed_slice();
        let mut nonce = [0; 16];
        File::open("/dev/urandom")?.read_exact(&mut nonce)?;
        let prefix = format!("carapace-bench-{}", std::process::id());
        let zero = Mapping::new(control, &format!("{prefix}-zero"), &Zero)?;
        for scute in 0..depth {
            let mut data = tempfile::tempfile()?;
            data.set_len(DATA_SIZE)?;
            let cow = attach(&data, false)?;
            let snapshot = Snapshot::from(Parameters {
                origin: zero.id,
                cow: DevId::try_from(cow.as_ref().metadata()?)?,
                persistence: Persistence::Persistent(Persistent::Standard),
                chunk_size: BlockSize::default(),
                discard: Discard::Disabled,
            });
            let mut mapping = Mapping::new(control, &format!("{prefix}-snapshot"), &snapshot)?;
            let output = mapping.open()?;
            // Full chunks need no prior origin data. Each scute overwrites block zero
            // and adds disjoint blocks that must be inherited by the final stack.
            let bytes = [u8::try_from(scute + 1)?; BLOCK as usize];
            output.write_all_at(&bytes, 0)?;
            for block in 0..WRITES {
                output.write_all_at(&bytes, (1 + scute * WRITES + block) as u64 * BLOCK)?;
            }
            output.sync_all()?;
            control.suspend(mapping.id, true)?;
            drop(output);
            // Synchronous removal guarantees metadata is settled before hashing.
            control.remove(mapping.id, false)?;
            mapping.removed = true;
            drop(mapping);
            drop(cow);
            let mut salt = root.to_vec();
            salt.extend_from_slice(&nonce);
            let header = Header {
                uuid: nonce,
                parameters: devmap_verity::Parameters {
                    hash_type: HashType::Normal,
                    algorithm: Algorithm::Sha256,
                    salt: Salt::new(&salt).ok_or("invalid salt length")?,
                    data: Geometry {
                        size: BlockSize::default(),
                        count: NonZero::new(DATA_SIZE / BLOCK).unwrap(),
                    },
                    hash: BlockSize::default(),
                },
            };
            let mut hash = tempfile::tempfile()?;
            root = header.format(&mut data, &mut hash)?;
            for (index, (source, bytes)) in [(&mut data, DATA_SIZE), (&mut hash, MIB)]
                .into_iter()
                .enumerate()
            {
                let number = disk.add_partition(
                    "carapace-bench",
                    bytes,
                    gpt::partition_types::LINUX_FS,
                    0,
                    Some(2048),
                )?;
                let mut partitions = disk.partitions().clone();
                let partition = partitions.get_mut(&number).unwrap();
                partition.part_guid =
                    uuid::Uuid::from_bytes_le(root[index * 16..(index + 1) * 16].try_into()?);
                image.seek(SeekFrom::Start(partition.first_lba * 512))?;
                source.rewind()?;
                io::copy(source, &mut image)?;
                disk.update_partitions(partitions)?;
            }
        }
        disk.write()?;
        image.sync_all()?;
        drop(zero);
        let device = attach(&image, true)?;
        let fixture = Self {
            root,
            _device: device,
            _image: image,
        };
        // Partition nodes can lag the synchronous kernel partition scan.
        let deadline = Instant::now() + Duration::from_secs(10);
        let chain = loop {
            match Chain::from_gpt(&fixture.root) {
                Ok(chain) => break chain,
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        };
        assert_eq!(chain.scutes.len(), depth);
        let file = chain.assemble()?;
        let mut bytes = [0; BLOCK as usize];
        file.read_exact_at(&mut bytes, 0)?;
        assert_eq!(bytes, [depth as u8; BLOCK as usize]);
        for scute in 0..depth {
            for block in 0..WRITES {
                file.read_exact_at(&mut bytes, (1 + scute * WRITES + block) as u64 * BLOCK)?;
                assert_eq!(bytes, [(scute + 1) as u8; BLOCK as usize]);
            }
        }
        file.read_exact_at(&mut bytes, (1 + depth * WRITES) as u64 * BLOCK)?;
        assert_eq!(bytes, [0; BLOCK as usize]);
        drop(file);
        drop(chain);
        wait_empty(control)?;
        Ok(fixture)
    }
}
