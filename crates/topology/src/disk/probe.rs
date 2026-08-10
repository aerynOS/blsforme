// SPDX-FileCopyrightText: Copyright © 2025 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

//! Disk probe/query APIs

use std::path::{Path, PathBuf};

use fs_err as fs;
use nix::sys::stat;
use snafu::{OptionExt, ResultExt as _};
use superblock::Superblock;

use super::{CanonicalizeSnafu, InvalidDeviceSnafu, IoSnafu, NixSnafu, device::BlockDevice, mounts::Table};

/// A Disk probe to query disks
#[derive(Debug)]
pub struct Probe {
    /// location of /sys
    pub(super) sysfs: PathBuf,

    /// location of /dev
    pub(super) devfs: PathBuf,

    /// location of /proc
    pub(super) procfs: PathBuf,

    /// Mountpoints
    pub mounts: Table,
}

impl Probe {
    /// Initial startup loads
    /// TODO: If requested, pvscan/vgscan/lvscan
    pub(super) fn init_scan(&mut self) -> Result<(), super::Error> {
        let mounts = Table::new_from_path(self.procfs.join("self").join("mounts")).context(IoSnafu)?;
        self.mounts = mounts;

        Ok(())
    }

    /// Resolve a device by mountpoint
    pub fn get_device_from_mountpoint(&self, mountpoint: impl AsRef<Path>) -> Result<PathBuf, super::Error> {
        let mountpoint = fs::canonicalize(mountpoint.as_ref()).context(IoSnafu)?;

        // Attempt to stat the device
        let stat = stat::lstat(&mountpoint).context(NixSnafu)?;
        let device_path =
            self.devfs
                .join("block")
                .join(format!("{}:{}", stat::major(stat.st_dev), stat::minor(stat.st_dev)));

        // Return by stat path if possible, otherwise fallback to mountpoint device
        if device_path.exists() {
            Ok(fs::canonicalize(&device_path).context(CanonicalizeSnafu)?)
        } else {
            // Find matching mountpoint
            let matching_device = self
                .mounts
                .iter()
                .find(|m| Path::new(m.mountpoint) == mountpoint)
                .ok_or(super::Error::UnknownMount { path: mountpoint })?;
            // TODO: Handle `ZFS=`, and composite bcachefs mounts (dev:dev1:dev2)
            Ok(matching_device.device.into())
        }
    }

    /// Retrieve the parent device, such as the disk of a partition, if possible
    pub fn get_device_parent(&self, device: impl AsRef<Path>) -> Option<PathBuf> {
        let device = fs::canonicalize(device.as_ref()).ok()?;
        let child = fs::canonicalize(
            device
                .file_name()
                .map(|f| self.sysfs.join("class").join("block").join(f))?,
        )
        .ok()?;
        let parent = child.parent()?.file_name()?;
        if parent == "block" {
            None
        } else {
            fs::canonicalize(self.devfs.join(parent)).ok()
        }
    }

    /// When given a path in `/dev` we attempt to resolve the full chain for it.
    /// Note: This does NOT include the initially passed device.
    pub fn get_device_chain(&self, device: impl AsRef<Path>) -> Result<Vec<PathBuf>, super::Error> {
        let device = fs::canonicalize(device.as_ref()).context(CanonicalizeSnafu)?;
        let sysfs_path = fs::canonicalize(
            device
                .file_name()
                .map(|f| self.sysfs.join("class").join("block").join(f))
                .context(InvalidDeviceSnafu { path: device })?,
        )
        .context(CanonicalizeSnafu)?;

        let mut ret = vec![];
        // no backing devices
        let dir = sysfs_path.join("slaves");
        if !dir.exists() {
            return Ok(ret);
        }

        // Build a recursive set of device backings
        for dir in fs::read_dir(dir).context(IoSnafu)? {
            let entry = dir.context(IoSnafu)?;
            let name = self.devfs.join(entry.file_name());
            ret.push(name.clone());
            ret.extend(self.get_device_chain(&name)?);
        }

        Ok(ret)
    }

    /// Scan superblock of the device for `UUID=` parameter
    pub fn get_device_superblock(&self, path: impl AsRef<Path>) -> Result<Superblock, super::Error> {
        let path = path.as_ref();
        log::trace!("Querying superblock information for {}", path.display());
        let mut fi = fs::File::open(path).context(IoSnafu)?;
        let sb = Superblock::from_reader(&mut fi)?;
        log::trace!("detected superblock: {}", sb.kind());

        Ok(sb)
    }

    /// Determine the composite rootfs device for the given mountpoint,
    /// building a set of superblocks and necessary `/proc/cmdline` arguments
    pub fn get_rootfs_device(&self, path: impl AsRef<Path>) -> Result<BlockDevice<'_>, super::Error> {
        let path = path.as_ref();
        let device = self.get_device_from_mountpoint(path)?;

        // Scan GPT for PartUUID
        let guid = self
            .get_device_parent(&device)
            .and_then(|parent| self.get_device_guid(parent, &device));

        let chain = self.get_device_chain(&device)?;
        let mut custodials = vec![device.clone()];
        custodials.extend(chain);

        let tip = custodials.pop().expect("we just added this..");
        let name = tip.to_string_lossy().to_string();

        let mut block = BlockDevice::new(self, &name, None, true)?;
        block.children = custodials
            .iter()
            .flat_map(|c| {
                if *c == device {
                    BlockDevice::new(self, c.clone(), Some(path.into()), false)
                } else {
                    BlockDevice::new(self, c.clone(), None, true)
                }
            })
            .collect::<Vec<_>>();
        block.guid = guid;

        Ok(block)
    }

    /// For GPT disks return the PartUUID (GUID)
    pub fn get_device_guid(&self, parent: PathBuf, path: &Path) -> Option<String> {
        let device = fs::canonicalize(path).ok()?;
        let sysfs_path = fs::canonicalize(
            device
                .file_name()
                .map(|f| self.sysfs.join("class").join("block").join(f))?,
        )
        .ok()?;
        let partition = str::parse::<u32>(fs::read_to_string(sysfs_path.join("partition")).ok()?.trim()).ok()?;
        let fi = fs::File::open(parent).ok()?;
        let gpt_header = gpt::GptConfig::new()
            .writable(false)
            .open_from_device(Box::new(fi))
            .ok()?;
        gpt_header
            .partitions()
            .get(&partition)
            .map(|partition| partition.part_guid.hyphenated().to_string())
    }
}
