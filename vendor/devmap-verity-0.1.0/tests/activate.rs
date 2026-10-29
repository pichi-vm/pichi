// SPDX-License-Identifier: Apache-2.0

//! Paths 3 and 4: activate header-backed and out-of-band verity volumes.

#![cfg(target_os = "linux")]

use std::{fs::OpenOptions, io::Read as _, num::NonZero};

use devmap_linux::{Control, DevId};
use devmap_verity::{Header, target::Verity};

use fixture::Fixture;

mod fixture {
    use std::{
        fs::File,
        io::{self, Write as _},
        os::unix::fs::PermissionsExt as _,
    };

    use devmap_core::{BlockSize, Detect as _, Geometry};
    use devmap_verity::{Algorithm, Constraint, Format as _, HashType, Header, Parameters, Salt};
    use lodown::{Configurable, Control as LoopControl, Device as LoopDevice, Writable};

    pub(super) struct Fixture {
        pub(super) mapping: Option<(devmap_linux::Control, devmap_linux::DevId)>,
        data_loop: File,
        hash_loop: File,
        root: Box<[u8]>,
        expected: Vec<u8>,
        // Drop after the open loop files so their backing paths remain present
        // throughout the loop-device lifetime.
        _directory: tempfile::TempDir,
    }

    impl Fixture {
        pub(super) fn new() -> io::Result<Self> {
            let directory = tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()?;
            let data_path = directory.path().join("data.img");
            let hash_path = directory.path().join("hash.img");
            let expected = vec![0x5a; 8 * 4096];

            let mut data = File::options()
                .write(true)
                .create_new(true)
                .open(&data_path)?;
            data.write_all(&expected)?;
            drop(data);

            let mut data = File::open(&data_path)?;
            let mut hash = File::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&hash_path)?;

            let header = Header {
                uuid: [0x5a; 16],
                parameters: Parameters {
                    hash_type: HashType::Normal,
                    algorithm: Algorithm::Sha256,
                    salt: Salt::default(),
                    data: Geometry::<Constraint>::detect(&data)?,
                    hash: BlockSize::<Constraint>::detect(&hash)?,
                },
            };

            let root = header.format(&mut data, &mut hash)?;
            hash.flush()?;
            drop(hash);

            let data_backing = File::open(data_path)?;
            let hash_backing = File::open(hash_path)?;
            let data_loop = Self::attach(&data_backing)?;
            let hash_loop = Self::attach(&hash_backing)?;

            Ok(Self {
                mapping: None,
                data_loop,
                hash_loop,
                root,
                expected,
                _directory: directory,
            })
        }

        fn attach(backing: &File) -> io::Result<File> {
            let control = LoopControl::open()?;
            loop {
                let number = control.get_free()?;
                let device = match LoopDevice::open(number) {
                    Ok(device) => device,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                let config = Configurable {
                    writable: Writable {
                        autoclear: true,
                        ..Writable::default()
                    },
                    read_only: true,
                    ..Configurable::default()
                };

                match device.configure(backing, 4096, config) {
                    Ok(()) => return File::open(format!("/dev/loop{number}")),
                    Err(error) if error.kind() == io::ErrorKind::ResourceBusy => {}
                    Err(error) => return Err(error),
                }
            }
        }

        pub(super) fn data(&self) -> &File {
            &self.data_loop
        }

        pub(super) fn hash(&self) -> &File {
            &self.hash_loop
        }

        pub(super) fn root(&self) -> &[u8] {
            &self.root
        }

        pub(super) fn expected(&self) -> &[u8] {
            &self.expected
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some((control, id)) = self.mapping.take() {
                let _ = control.remove(id, true);
            }
        }
    }
}

#[test]
#[ignore = "requires root and real loop/device-mapper devices"]
fn activates_from_an_on_disk_header() {
    // Set up the verity fixture and open the control device.
    let mut fixture = Fixture::new().expect("create verity fixture");
    let control = Control::open().expect("open /dev/mapper/control");

    // Get the device numbers for the backing data and hash loop devices.
    let data = DevId::try_from(fixture.data().metadata().unwrap()).unwrap();
    let hash = DevId::try_from(fixture.hash().metadata().unwrap()).unwrap();

    // Read the on-disk header from the hash loop device.
    let mut record = [0; 512];
    fixture
        .hash()
        .try_clone()
        .expect("clone hash loop device")
        .read_exact(&mut record)
        .expect("read on-disk header");
    let header = Header::decode(record).expect("decode on-disk header");

    // Create the verity target from the on-disk header.
    let target = header.builder(data, hash, fixture.root()).unwrap().build();

    // Activate the target from the on-disk header.
    let name = format!("devmap-verity-header-{}", std::process::id());
    let id = control.create(&name).unwrap();
    fixture.mapping = Some((control.clone(), id));
    control
        .load(
            id,
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, target.data_sectors()),
                    &target,
                    control.version("verity").unwrap(),
                )
                .unwrap(),
            true,
        )
        .unwrap();
    control.suspend(id, false).unwrap();

    let mut file = OpenOptions::new()
        .read(true)
        .open(format!("/dev/dm-{}", id.minor()))
        .unwrap();
    control.remove(id, true).unwrap();
    let mut actual = Vec::new();
    file.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, fixture.expected());
}

#[test]
#[ignore = "requires root and real loop/device-mapper devices"]
fn activates_from_out_of_band_parameters() {
    // Set up the verity fixture and open the control device.
    let mut fixture = Fixture::new().expect("create verity fixture");
    let control = Control::open().expect("open /dev/mapper/control");

    // Get the device numbers for the backing data and hash loop devices.
    let data = DevId::try_from(fixture.data().metadata().unwrap()).unwrap();
    let hash = DevId::try_from(fixture.hash().metadata().unwrap()).unwrap();

    // Create the verity target without reading the header. The tree starts
    // after the header block, but every parameter is supplied out of band.
    let target = Verity::builder(data, hash, NonZero::new(8).unwrap(), fixture.root())
        .unwrap()
        .hash_start(1)
        .unwrap()
        .build();

    // Activate the target from the out-of-band parameters.
    let name = format!("devmap-verity-params-{}", std::process::id());
    let id = control.create(&name).unwrap();
    fixture.mapping = Some((control.clone(), id));
    control
        .load(
            id,
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, target.data_sectors()),
                    &target,
                    control.version("verity").unwrap(),
                )
                .unwrap(),
            true,
        )
        .unwrap();
    control.suspend(id, false).unwrap();

    let mut file = OpenOptions::new()
        .read(true)
        .open(format!("/dev/dm-{}", id.minor()))
        .unwrap();
    control.remove(id, true).unwrap();
    let mut actual = Vec::new();
    file.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, fixture.expected());
}
