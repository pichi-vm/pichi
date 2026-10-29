// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

fn source(bytes: &[u8]) -> File {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(bytes).unwrap();
    file
}

fn plan(file: File) -> Planned<File> {
    <Planned<File> as Plan>::plan(file).unwrap()
}

fn collect(mut plan: Planned<File>, read_size: usize) -> Vec<u8> {
    let size = plan.size();
    let mut bytes = Vec::new();
    let mut buffer = vec![0; read_size];
    loop {
        let n = plan.read(&mut buffer).unwrap();
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..n]);
    }
    assert_eq!(bytes.len() as u64, size);
    assert_eq!(plan.size(), size);
    assert_eq!(plan.read(&mut buffer).unwrap(), 0);
    bytes
}

// Independently decode the actual wire records over a zero origin.
fn reconstruct(wire: &[u8], size: usize) -> (Vec<u8>, usize) {
    assert_eq!(&wire[..4], b"SnAp");
    assert_eq!(u32::from_le_bytes(wire[12..16].try_into().unwrap()), 8);
    let mut result = vec![0; size.div_ceil(BLOCK) * BLOCK];
    let mut count = 0;
    let mut area = 1;
    loop {
        for record in wire[area * BLOCK..(area + 1) * BLOCK].as_chunks::<16>().0 {
            let old = u64::from_le_bytes(record[..8].try_into().unwrap()) as usize;
            let new = u64::from_le_bytes(record[8..].try_into().unwrap()) as usize;
            if new == 0 {
                result.truncate(size);
                return (result, count);
            }
            assert_ne!(new % 257, 1);
            result[old * BLOCK..(old + 1) * BLOCK]
                .copy_from_slice(&wire[new * BLOCK..(new + 1) * BLOCK]);
            count += 1;
        }
        area += 257;
    }
}

fn fixture(count: usize) -> Vec<u8> {
    let mut bytes = vec![0; (count + 2) * BLOCK + 17];
    for i in 0..count {
        bytes[(i + 1) * BLOCK..(i + 2) * BLOCK].fill(0x73);
    }
    if count != 0 {
        *bytes.last_mut().unwrap() = 0x42;
    }
    bytes
}

#[test]
fn reconstructs_zero_partial_and_metadata_boundaries() {
    for count in [0, 1, 254, 255, 256, 257, 511, 512] {
        let bytes = fixture(count);
        let planned = plan(source(&bytes));
        let expected = count + usize::from(count != 0);
        assert_eq!(planned.exceptions(), expected as u64);
        let wire = collect(planned, 7777);
        assert_eq!(reconstruct(&wire, bytes.len()), (bytes, expected));
    }
}

#[test]
fn full_area_terminator_and_fragmented_reads() {
    let bytes = vec![1; 256 * BLOCK];
    let wire = collect(plan(source(&bytes)), 19);
    assert_eq!(wire.len(), 259 * BLOCK);
    assert!(wire[258 * BLOCK..].iter().all(|v| *v == 0));
    assert_eq!(reconstruct(&wire, bytes.len()), (bytes, 256));
    let bytes = fixture(1);
    assert_eq!(
        collect(plan(source(&bytes)), 1),
        collect(plan(source(&bytes)), 65536)
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sparse() -> File {
    let mut image = tempfile::tempfile().unwrap();
    image.set_len(1 << 34).unwrap();
    image.seek(SeekFrom::Start(3 * BLOCK as u64)).unwrap();
    image.write_all(&[0x59; BLOCK]).unwrap();
    image
        .seek(SeekFrom::Start((1 << 34) - BLOCK as u64))
        .unwrap();
    image.write_all(&[0; BLOCK]).unwrap();
    image
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn planning_skips_holes_and_omits_allocated_zero_blocks() {
    let source = sparse();
    assert!(source.metadata().unwrap().blocks() * 512 < (1 << 20));
    let planned = plan(source);
    assert_eq!(planned.exceptions(), 1);
    assert!(planned.metadata.read_bytes <= 1024 * 1024);
    let wire = collect(planned, 65536);
    assert_eq!(wire.len(), 3 * BLOCK);
    assert_eq!(
        Exception::decode(wire[BLOCK..BLOCK + 16].try_into().unwrap()),
        Exception {
            old_chunk: 3,
            new_chunk: 2
        }
    );
    assert!(wire[2 * BLOCK..].iter().all(|b| *b == 0x59));
}

#[test]
fn dense_and_sparse_representations_agree() {
    let mut sparse = tempfile::tempfile().unwrap();
    sparse.set_len(16 * BLOCK as u64 + 7).unwrap();
    sparse.seek(SeekFrom::Start(3 * BLOCK as u64)).unwrap();
    sparse.write_all(&[0x37; BLOCK]).unwrap();
    sparse.seek(SeekFrom::Start(16 * BLOCK as u64 + 6)).unwrap();
    sparse.write_all(&[0x52]).unwrap();
    let mut bytes = vec![0; 16 * BLOCK + 7];
    sparse.rewind().unwrap();
    sparse.read_exact(&mut bytes).unwrap();
    assert_eq!(
        collect(plan(sparse), 1024),
        collect(plan(source(&bytes)), 1024)
    );
}

#[test]
fn validates_source_and_propagates_second_pass_failure() {
    assert!(<Planned<File> as Plan>::plan(tempfile::tempfile().unwrap()).is_err());
    #[cfg(unix)]
    assert!(<Planned<File> as Plan>::plan(File::open(".").unwrap()).is_err());
    // Unix set_len creates a sparse file without allocating 64 GiB.
    #[cfg(unix)]
    {
        let file = tempfile::tempfile().unwrap();
        file.set_len(crate::ZERO_COUNT_SECTORS * 512 + 1).unwrap();
        assert!(<Planned<File> as Plan>::plan(file)
            .unwrap_err()
            .to_string()
            .contains("64 GiB"));
    }
    let file = source(&[1; BLOCK]);
    let mut planned = plan(file.try_clone().unwrap());
    // Mutation violates the contract, but truncation must surface as an error,
    // never get mistaken for sparse zeros or successful EOF.
    file.set_len(0).unwrap();
    let mut bytes = Vec::new();
    assert_eq!(
        planned.read_to_end(&mut bytes).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert_eq!(bytes.len(), 2 * BLOCK);
    assert!(planned.read(&mut [0]).is_err());
    assert_eq!(planned.read(&mut []).unwrap(), 0);
}

fn verity(size: u64) -> devmap_verity::Header {
    devmap_verity::Header {
        uuid: [0; 16],
        parameters: devmap_verity::Parameters {
            algorithm: devmap_verity::Algorithm::Sha256,
            hash_type: devmap_verity::HashType::Normal,
            salt: devmap_verity::Salt::new(&[0; 32]).unwrap(),
            data: devmap_core::Geometry {
                size: BlockSize::default(),
                count: NonZero::new(size / BLOCK as u64).unwrap(),
            },
            hash: BlockSize::default(),
        },
    }
}

struct Tee<R, W>(R, W);
impl<R: Read, W: Write> Read for Tee<R, W> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = self.0.read(out)?;
        self.1.write_all(&out[..n])?;
        Ok(n)
    }
}

#[test]
fn feeds_existing_verity_formatter_with_known_length() {
    let planned = plan(source(&fixture(256)));
    let header = verity(planned.size());
    let mut cow = Vec::new();
    let mut hash = std::io::Cursor::new(Vec::new());
    let root = devmap_verity::Format::format(header, Tee(planned, &mut cow), &mut hash).unwrap();
    let mut reference = std::io::Cursor::new(Vec::new());
    let expected = devmap_verity::Format::format(header, cow.as_slice(), &mut reference).unwrap();
    assert_eq!(root, expected);
    assert_eq!(hash.into_inner(), reference.into_inner());
}

#[cfg(feature = "tokio")]
#[test]
fn async_sparse_plans_and_streams_match_blocking() {
    use tokio::io::AsyncReadExt as _;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        for count in [0, 1, 254, 255, 256, 257, 511, 512] {
            let bytes = fixture(count);
            let mut planned = <Planned<tokio::fs::File> as AsyncPlan>::plan(
                tokio::fs::File::from_std(source(&bytes)),
            )
            .await
            .unwrap();
            assert_eq!(
                planned.exceptions(),
                (count + usize::from(count != 0)) as u64
            );
            let mut wire = Vec::new();
            let mut buffer = vec![0; 7777];
            assert_eq!(planned.read(&mut []).await.unwrap(), 0);
            loop {
                let n = planned.read(&mut buffer).await.unwrap();
                if n == 0 {
                    break;
                }
                wire.extend_from_slice(&buffer[..n]);
            }
            assert_eq!(wire, collect(plan(source(&bytes)), 1 + BLOCK));
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let mut planned =
                <Planned<tokio::fs::File> as AsyncPlan>::plan(tokio::fs::File::from_std(sparse()))
                    .await
                    .unwrap();
            assert_eq!(planned.exceptions(), 1);
            assert!(planned.metadata.read_bytes <= 1024 * 1024);
            let mut wire = Vec::new();
            planned.read_to_end(&mut wire).await.unwrap();
            assert_eq!(wire, collect(plan(sparse()), 65536));
        }
    });
}

#[cfg(feature = "tokio")]
#[test]
fn async_verity_and_source_failure() {
    use tokio::io::AsyncReadExt as _;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let bytes = fixture(255);
        let planned = <Planned<tokio::fs::File> as AsyncPlan>::plan(tokio::fs::File::from_std(
            source(&bytes),
        ))
        .await
        .unwrap();
        let header = verity(planned.size());
        let mut hash = std::io::Cursor::new(Vec::new());
        let root = devmap_verity::AsyncFormat::format(header, planned, &mut hash)
            .await
            .unwrap();
        let mut expected_hash = std::io::Cursor::new(Vec::new());
        let expected_root =
            devmap_verity::Format::format(header, plan(source(&bytes)), &mut expected_hash)
                .unwrap();
        assert_eq!(root, expected_root);
        assert_eq!(hash.into_inner(), expected_hash.into_inner());
        let file = source(&[1; BLOCK]);
        let mut planned = <Planned<tokio::fs::File> as AsyncPlan>::plan(tokio::fs::File::from_std(
            file.try_clone().unwrap(),
        ))
        .await
        .unwrap();
        file.set_len(0).unwrap();
        assert_eq!(
            planned
                .read_to_end(&mut Vec::new())
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(planned.read(&mut [0]).await.is_err());
        assert_eq!(planned.read(&mut []).await.unwrap(), 0);
    });
}

#[cfg(feature = "tokio")]
#[test]
fn async_constructor_is_send_and_lazy() {
    fn send<T: Send>(value: T) -> T {
        value
    }
    use std::io::Seek;
    let mut file = source(&[1; BLOCK]);
    let position = file.stream_position().unwrap();
    let future = <Planned<tokio::fs::File> as AsyncPlan>::plan(tokio::fs::File::from_std(
        file.try_clone().unwrap(),
    ));
    drop(send(future));
    assert_eq!(file.stream_position().unwrap(), position);
}

#[test]
#[ignore = "requires root, loop devices, and device mapper"]
#[cfg(all(target_os = "linux", feature = "loop"))]
fn kernel_reads_planned_scute() {
    use crate::Assemble as _;
    use std::io::Seek as _;
    let bytes = fixture(257);
    let planned = plan(source(&bytes));
    let mut cow = tempfile::tempfile().unwrap();
    let mut hash = tempfile::tempfile().unwrap();
    let root =
        devmap_verity::Format::format(verity(planned.size()), Tee(planned, &mut cow), &mut hash)
            .unwrap();
    cow.rewind().unwrap();
    hash.rewind().unwrap();
    let chain = crate::Chain {
        root,
        scutes: vec![crate::Scute { data: cow, hash }],
    };
    let mut device = chain.assemble().unwrap();
    let mut actual = vec![0; bytes.len()];
    device.read_exact(&mut actual).unwrap();
    assert_eq!(actual, bytes);
}
