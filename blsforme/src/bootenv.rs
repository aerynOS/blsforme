// SPDX-FileCopyrightText: Copyright © 2025 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

//! Boot environment tracking (ESP vs XBOOTLDR, etc)

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use fs_err as fs;
use gpt::{GptConfig, partition_types};
use snafu::ResultExt as _;
use topology::disk::probe::Probe;

use crate::{
    Configuration, Error, GptSnafu, IoSnafu, Root,
    bootloader::systemd_boot::interface::{BootLoaderInterface, VariableName},
};

/// Type of firmware detected
///
/// By knowing the available firmware (effectively: is `efivarfs` mounted)
/// we can detect full availability of UEFI features or legacy fallback.
#[derive(Debug, PartialEq)]
pub enum Firmware {
    /// UEFI
    Uefi,

    /// Legacy BIOS. Tread carefully
    Bios,
}

/// Helps access the boot environment, ie `$BOOT` and specific ESP
#[derive(Debug)]
pub struct BootEnvironment {
    /// xbootldr device
    pub xbootldr: Option<PathBuf>,

    /// The EFI System Partition (stored as a device path)
    pub esp: Option<PathBuf>,

    /// Firmware in use
    pub firmware: Firmware,

    pub(crate) esp_mountpoint: Option<PathBuf>,
    pub(crate) xboot_mountpoint: Option<PathBuf>,
}

impl BootEnvironment {
    /// Return a new BootEnvironment for the given root
    pub fn new(probe: &Probe, disk_parent: Option<PathBuf>, config: &Configuration) -> Result<Self, Error> {
        let firmware = if config.vfs.join("sys").join("firmware").join("efi").exists() {
            Firmware::Uefi
        } else {
            Firmware::Bios
        };

        let mounts = probe
            .mounts
            .iter()
            .filter_map(|m| Some((fs::canonicalize(m.device).ok()?, m)))
            .collect::<HashMap<_, _>>();

        let esp_from_bls = match config.root {
            // For image mode, don't query BLS.
            Root::Image(_) => None,
            // Otherwise, query BLS first.
            _ => Self::determine_esp_by_bls(&firmware, config).ok(),
        };

        // If in image mode or if the BLS query failed, use raw discovery of the GPT device.
        let esp = esp_from_bls.or_else(|| Self::determine_esp_by_gpt(&disk_parent?, config).ok());

        // Make sure our config is sane!
        if firmware == Firmware::Uefi && esp.is_none() {
            log::error!("No usable ESP detected for a UEFI system");
            return Err(Error::NoEsp);
        }

        let Some(esp_path) = &esp else {
            return Ok(Self {
                xbootldr: None,
                esp,
                firmware,
                xboot_mountpoint: None,
                esp_mountpoint: None,
            });
        };

        let esp_mountpoint = mounts.get(esp_path).and_then(|m| fs::canonicalize(m.mountpoint).ok());

        // Report ESP and check for XBOOTLDR
        log::info!("EFI System Partition: {}", esp_path.display());

        let xbootldr = Self::discover_xbootldr(probe, esp_path, config).ok();
        if let Some(path) = &xbootldr {
            log::info!("EFI XBOOTLDR Partition: {}", path.display());
        }

        let xboot_mountpoint = xbootldr
            .as_ref()
            .and_then(|e| fs::canonicalize(mounts.get(e)?.mountpoint).ok());

        Ok(Self {
            xbootldr,
            esp,
            firmware,
            xboot_mountpoint,
            esp_mountpoint,
        })
    }

    /// If UEFI we can ask BootLoaderProtocol for help to find out the ESP device.
    fn determine_esp_by_bls(firmware: &Firmware, config: &Configuration) -> Result<PathBuf, Error> {
        // UEFI only tyvm
        if let Firmware::Bios = *firmware {
            return Err(Error::Unsupported);
        }

        let systemd = BootLoaderInterface::new(&config.vfs)?;
        let info = systemd.get_ucs2_string(VariableName::Info)?;
        log::trace!("Encountered BLS compatible bootloader: {info}");
        Ok(systemd.get_device_path()?)
    }

    /// Determine ESP by searching relative GPT
    fn determine_esp_by_gpt(disk_parent: &Path, config: &Configuration) -> Result<PathBuf, Error> {
        log::trace!("Finding ESP on device: {disk_parent:?}");
        let table = GptConfig::new().writable(false).open(disk_parent).context(GptSnafu)?;
        let (_, esp) = table
            .partitions()
            .iter()
            .find(|(_, p)| p.part_type_guid == partition_types::EFI)
            .ok_or(Error::NoEsp)?;
        let path = config
            .vfs
            .join("dev")
            .join("disk")
            .join("by-partuuid")
            .join(esp.part_guid.as_hyphenated().to_string());
        fs::canonicalize(path).context(IoSnafu)
    }

    /// Discover an XBOOTLDR partition *relative* to wherever the ESP is
    fn discover_xbootldr(probe: &Probe, esp: &PathBuf, config: &Configuration) -> Result<PathBuf, Error> {
        let parent = probe.get_device_parent(esp).ok_or(Error::Unsupported)?;
        log::trace!("Finding XBOOTLDR on device: {parent:?}");
        let table = GptConfig::new().writable(false).open(parent).context(GptSnafu)?;
        let (_, esp) = table
            .partitions()
            .iter()
            .find(|(_, p)| p.part_type_guid == partition_types::FREEDESK_BOOT)
            .ok_or(Error::NoXbootldr)?;
        let path = config
            .vfs
            .join("dev")
            .join("disk")
            .join("by-partuuid")
            .join(esp.part_guid.as_hyphenated().to_string());
        fs::canonicalize(path).context(IoSnafu)
    }

    /// The so-called `$BOOT` partition (UEFI only at present)
    pub fn boot_partition(&self) -> Option<&PathBuf> {
        self.xbootldr().or_else(|| self.esp())
    }

    /// Return the EFI System Partition (UEFI only)
    pub fn esp(&self) -> Option<&PathBuf> {
        self.esp.as_ref()
    }

    /// Return the XBOOTLDR partition (UEFI only)
    pub fn xbootldr(&self) -> Option<&PathBuf> {
        self.xbootldr.as_ref()
    }
}
