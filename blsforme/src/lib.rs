// SPDX-FileCopyrightText: Copyright © 2025 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

use std::path::PathBuf;

use bootloader::systemd_boot;
use gpt::GptError;
use snafu::Snafu;

/// Re-export the topology APIs
pub use topology::disk;

pub use self::bootenv::{BootEnvironment, Firmware};
pub use self::cmdline::GlobalCmdline;
pub use self::entry::Entry;
pub use self::kernel::{AuxiliaryFile, AuxiliaryKind, BootJSON, Kernel, Schema};
pub use self::manager::Manager;

mod bootenv;
mod cmdline;
mod entry;
mod kernel;
mod manager;

pub mod bootloader;
pub mod file_utils;
pub mod os_release;

/// Core error type for blsforme
#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(context(false), display("boot loader protocol: {source}"))]
    BootLoaderProtocol { source: systemd_boot::interface::Error },

    #[snafu(context(false), display("bootloader error"))]
    Bootloader { source: bootloader::Error },

    #[snafu(display("c stdlib: {source}"))]
    Nix { source: nix::errno::Errno },

    #[snafu(display("undetected xbootldr"))]
    NoXbootldr,

    #[snafu(display("undetected ESP"))]
    NoEsp,

    #[snafu(display("failed to interact with filesystem properly"))]
    InvalidFilesystem,

    #[snafu(display("generic i/o error"))]
    Io { source: std::io::Error },

    #[snafu(display("GPT error"))]
    Gpt { source: GptError },

    #[snafu(context(false), display("topology scan: {source}"))]
    Topology { source: topology::disk::Error },

    #[snafu(display("no ESP mounted in image mode, but detected an ESP at {path:?}"))]
    UnmountedEsp { path: PathBuf },

    #[snafu(display("unsupported usage"))]
    Unsupported,
}

/// Core configuration for boot management
#[derive(Debug)]
pub struct Configuration {
    /// Root of all operations
    pub root: Root,

    /// Where we can find `sysfs` `proc` etc
    pub vfs: PathBuf,
}

/// Wrap a root into a strong type to avoid confusion
#[derive(Debug)]
pub enum Root {
    /// Native installation
    Native(PathBuf),

    /// Image generation
    Image(PathBuf),
}

impl Root {
    /// When we don't need the type of the root..
    pub fn path(&self) -> &PathBuf {
        match self {
            Root::Native(p) => p,
            Root::Image(p) => p,
        }
    }
}
