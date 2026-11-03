// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use aya::{
    Btf, Ebpf,
    programs::{Lsm, links::FdLink},
};
use std::error::Error;

const POLICY: &[u8] =
    aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/conglobate-label-policy"));

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let [pin] = arguments.as_slice() else {
        return Err("usage: conglobate-lsm PIN".into());
    };
    let mut bpf = Ebpf::load(POLICY)?;
    let program: &mut Lsm = bpf
        .program_mut("skip_label_cap")
        .ok_or("object does not contain skip_label_cap")?
        .try_into()?;
    program.load("inode_xattr_skipcap", &Btf::from_sys_fs()?)?;
    let link = program.attach()?;
    let link: FdLink = program.take_link(link)?.into();
    // The bpffs pin retains the attachment after this process exits.
    link.pin(pin)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires root and BPF LSM support"]
    fn kernel_verifies_the_policy_without_attaching_it() -> Result<(), Box<dyn Error>> {
        let mut bpf = Ebpf::load(POLICY)?;
        let program: &mut Lsm = bpf
            .program_mut("skip_label_cap")
            .ok_or("object does not contain skip_label_cap")?
            .try_into()?;
        program.load("inode_xattr_skipcap", &Btf::from_sys_fs()?)?;
        Ok(())
    }
}
