// SPDX-FileCopyrightText: Copyright © 2025 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

//! Bootloader APIs

use std::path::{PathBuf, StripPrefixError};

use snafu::Snafu;

use crate::{Entry, Firmware, GlobalCmdline, Kernel, Schema, manager::Mounts};

pub mod systemd_boot;

/// Bootloader errors
#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("missing bootloader file: {filename}"))]
    MissingFile { filename: &'static str },

    #[snafu(display("missing mountpoint: {description}"))]
    MissingMount { description: &'static str },

    #[snafu(display("io: {source}"))]
    Io { source: std::io::Error },

    #[snafu(display("wip: {source}"))]
    Prefix { source: StripPrefixError },
}

#[derive(Debug)]
pub enum Bootloader<'a, 'b> {
    /// We really only support systemd-boot right now
    Systemd(Box<systemd_boot::Loader<'a, 'b>>),
}

impl<'a, 'b> Bootloader<'a, 'b> {
    /// Construct the firmware-appropriate bootloader manager
    pub(crate) fn new(
        schema: &'a Schema,
        assets: &'b [PathBuf],
        mounts: &'a Mounts,
        firmware: &Firmware,
    ) -> Result<Self, Error> {
        match firmware {
            Firmware::Uefi => Ok(Bootloader::Systemd(Box::new(systemd_boot::Loader::new(
                schema, assets, mounts,
            )?))),
            Firmware::Bios => unimplemented!(),
        }
    }

    /// Sync bootloader to BOOT dir
    pub fn sync(&self) -> Result<(), Error> {
        match &self {
            Bootloader::Systemd(s) => s.sync(),
        }
    }

    pub fn sync_entries(&self, cmdline: &GlobalCmdline, entries: &[Entry]) -> Result<(), Error> {
        match &self {
            Bootloader::Systemd(s) => s.sync_entries(cmdline, entries),
        }
    }

    /// Grab the installed entries
    pub fn installed_kernels(&self) -> Result<Vec<Kernel>, Error> {
        match &self {
            Bootloader::Systemd(s) => s.installed_kernels(),
        }
    }
}
