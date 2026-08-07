// SPDX-FileCopyrightText: Copyright © 2025 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

//! Boot loader management entry APIs

use std::path::{Path, PathBuf};

use fs_err as fs;
use nix::mount::{MsFlags, mount, umount};
use snafu::{ResultExt as _, ensure};
use topology::disk;

use crate::{
    BootEnvironment, Configuration, Entry, Error, GlobalCmdline, IoSnafu, Kernel, NixSnafu, Root, Schema,
    UnmountedEspSnafu, bootloader::Bootloader,
};

#[derive(Debug)]
pub(crate) struct Mounts {
    pub(crate) xbootldr: Option<PathBuf>,
    pub(crate) esp: Option<PathBuf>,
}

/// Encapsulate the entirety of the boot management core APIs
#[derive(Debug)]
pub struct Manager<'a> {
    config: &'a Configuration,

    /// OS provided kernels
    entries: Vec<Entry<'a>>,

    /// Potential bootloader assets, allow impl to filter for right paths
    bootloader_assets: Vec<PathBuf>,

    /// Our detected boot environment
    boot_env: BootEnvironment,

    mounts: Mounts,

    global_cmdline: GlobalCmdline,
}

impl<'a> Manager<'a> {
    /// Construct a new blsforme::Manager with the given configuration
    pub fn new(config: &'a Configuration) -> Result<Self, Error> {
        // Probe the rootfs device managements
        let probe = disk::Builder::default().build()?;
        let root = probe.get_rootfs_device(config.root.path())?;

        // Construct new global cmdline from the root block device
        // and loaded /etc snippets.
        let global_cmdline = GlobalCmdline::new(config, &root);

        // Grab parent disk, establish disk environment setup
        let disk_parent = probe.get_device_parent(root.path);
        let boot_env = BootEnvironment::new(&probe, disk_parent, config)?;
        log::trace!("boot env: {boot_env:?}");

        let mut mounts = Mounts {
            xbootldr: boot_env.xboot_mountpoint.clone().or_else(|| {
                _ = boot_env.xbootldr()?;
                Some(config.root.path().join("boot"))
            }),
            esp: boot_env.esp_mountpoint.clone().or_else(|| {
                _ = boot_env.esp()?;
                Some(config.root.path().join("efi"))
            }),
        };

        log::trace!("selected mountpoints: {mounts:?}");

        // So, we got a `/boot` mount for ESP, legacy style. We can't stick xbootldr there...
        if let Some(xbootldr) = mounts.xbootldr.as_ref() {
            if let Some(esp) = mounts.esp.as_ref() {
                if esp == xbootldr && boot_env.xbootldr().is_none() {
                    mounts.xbootldr = Some(config.root.path().join("xboot"))
                }
            }
        }

        Ok(Self {
            config,
            entries: vec![],
            bootloader_assets: vec![],
            boot_env,
            mounts,
            global_cmdline,
        })
    }

    /// Access the global cmdline
    pub fn global_cmdline(&self) -> String {
        self.global_cmdline.cmdline()
    }

    /// Set the system kernels to use for sync operations
    pub fn with_entries(self, entries: impl IntoIterator<Item = Entry<'a>>) -> Self {
        Self {
            entries: entries.into_iter().collect::<Vec<_>>(),
            ..self
        }
    }

    /// Update the set of bootloader assets
    pub fn with_bootloader_assets(self, assets: Vec<PathBuf>) -> Self {
        Self {
            bootloader_assets: assets,
            ..self
        }
    }

    /// Mount any required partitions (ESP/XBOOTLDR)
    pub fn mount_partitions(&self) -> Result<Vec<ScopedMount>, Error> {
        let mut mounted_paths = vec![];

        // Stop silly buggers with image based mounting
        if let Root::Image(_) = self.config.root {
            log::warn!("Refusing to auto-mount partitions in image mode");
            return Ok(mounted_paths);
        }

        // Got the ESP, not mounted.
        if let Some(hw) = self.boot_env.esp() {
            if self.boot_env.esp_mountpoint.is_none() {
                let mount_point = self.mounts.esp.clone().ok_or(Error::NoEsp)?;
                mounted_paths.insert(0, self.mount_vfat_partition(hw, &mount_point)?);
            }
        }
        // Got an XBOOTLDR, not mounted..
        if let Some(hw) = self.boot_env.xbootldr() {
            if self.boot_env.xboot_mountpoint.is_none() {
                let mount_point = self.mounts.xbootldr.clone().ok_or(Error::NoXbootldr)?;
                mounted_paths.insert(0, self.mount_vfat_partition(hw, &mount_point)?);
            }
        }

        Ok(mounted_paths)
    }

    /// Returns the boot environment
    pub fn boot_environment(&self) -> &BootEnvironment {
        &self.boot_env
    }

    /// Discover installed kernels using the mount tokens
    pub fn installed_kernels(&self, schema: &Schema, _tokens: &[ScopedMount]) -> Result<Vec<Kernel>, Error> {
        let bootloader = self.bootloader(schema)?;
        let results = bootloader.installed_kernels()?;
        Ok(results)
    }

    /// Mount an fat filesystem
    #[inline]
    fn mount_vfat_partition(&self, source: &Path, target: &Path) -> Result<ScopedMount, Error> {
        let options: Option<&str> = None;
        if !target.exists() {
            fs::create_dir_all(target).context(IoSnafu)?;
        }
        mount(Some(source), target, Some("vfat"), MsFlags::MS_MGC_VAL, options).context(NixSnafu)?;
        log::info!("Mounted vfat partition {} at {}", source.display(), target.display());
        Ok(ScopedMount {
            point: target.into(),
            mounted: true,
        })
    }

    /// Attempt to sync kernels/bootloader with the targets
    ///
    /// Any already installed kernels will be skipped, and this step
    /// is not responsible for *deleting* any unused kernels
    pub fn sync(&self, schema: &Schema) -> Result<(), Error> {
        if let Root::Image(_) = self.config.root {
            if let Some(esp) = self.boot_env.esp() {
                ensure!(self.boot_env.esp_mountpoint.is_some(), UnmountedEspSnafu { path: esp });
            }
        }
        // Firstly, get the bootloader updated.
        let bootloader = self.bootloader(schema)?;
        bootloader.sync()?;

        // Sync the entries
        bootloader.sync_entries(&self.global_cmdline, &self.entries)?;

        Ok(())
    }

    /// factory - create bootloader instance
    fn bootloader(&'a self, schema: &'a Schema) -> Result<Bootloader<'a, 'a>, Error> {
        Ok(Bootloader::new(
            schema,
            &self.bootloader_assets,
            &self.mounts,
            &self.boot_env.firmware,
        )?)
    }
}

/// Encapsulated mountpoint to ensure auto-unmount (Scoped)
pub struct ScopedMount {
    point: PathBuf,
    mounted: bool,
}

impl Drop for ScopedMount {
    fn drop(&mut self) {
        if !self.mounted {
            return;
        }
        self.mounted = true;
        match umount(&self.point) {
            Ok(_) => log::info!("Unmounted {}", self.point.display()),
            Err(err) => log::error!("Failed to umount {}: {}", self.point.display(), err),
        }
    }
}
