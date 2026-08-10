// SPDX-FileCopyrightText: Copyright © 2025 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

//! Loose encapsulation of superblock APIs mapped into path'd devices
//! These APIs need to exist as a safety mechanism in order to not load
//! a bunch of superblocks dynamically allocated into memory..

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use crate::disk::mounts::MountOption;

use super::probe;

pub struct BlockDevice<'a> {
    pub kind: Option<superblock::Kind>,

    // Actively mounted somewhere?
    pub mountpoint: Option<PathBuf>,

    // Generally easier to work with strings in client code
    pub path: String,

    /// Block devices living under this device..
    pub children: Vec<BlockDevice<'a>>,

    /// What owns us, precious.
    pub(super) probe: &'a probe::Probe,

    // Superblock's UUID
    pub(super) uuid: Option<String>,

    // GPT partition GUID
    pub(super) guid: Option<String>,

    // Auxiliary (ignored) device
    pub(super) aux: bool,
}

impl<'a> BlockDevice<'a> {
    pub(super) fn new(
        probe: &'a probe::Probe,
        path: impl AsRef<Path>,
        mount: Option<PathBuf>,
        aux: bool,
    ) -> Result<Self, super::Error> {
        let path = path.as_ref();

        let block = if let Result::Ok(sb) = probe.get_device_superblock(path) {
            BlockDevice {
                kind: Some(sb.kind()),
                mountpoint: mount.clone(),
                path: path.to_string_lossy().to_string(),
                children: vec![],
                probe,
                uuid: Some(sb.uuid()?),
                guid: None,
                aux,
            }
        } else {
            BlockDevice {
                kind: None,
                mountpoint: mount.clone(),
                path: path.to_string_lossy().to_string(),
                children: vec![],
                probe,
                uuid: None,
                guid: None,
                aux,
            }
        };
        Ok(block)
    }

    /// Generate a working "root=" style boot line
    pub fn cmd_line(&self) -> String {
        let children = self.children.iter().map(|c| c.cmd_line()).collect::<Vec<_>>().join(" ");
        let mounts = self
            .probe
            .mounts
            .iter()
            .map(|m| (PathBuf::from(&m.mountpoint), m))
            .collect::<HashMap<_, _>>();
        let mount = self.mountpoint.as_ref().and_then(|m| mounts.get(m));
        let mount_options = if let Some(mp) = mount {
            mp.options()
                .filter_map(|o| match o {
                    MountOption::Option(k, v) => Some((k, v)),
                    _ => None,
                })
                .collect::<HashMap<_, _>>()
        } else {
            HashMap::new()
        };

        let local = if let Some(kind) = &self.kind {
            match kind {
                superblock::Kind::Btrfs => {
                    let uuid = self.uuid.as_ref().expect("cannot have btrfs without uuid..");
                    if let Some(subvol) = mount_options.get("subvol") {
                        format!("root=UUID={uuid} rootfsflags=subvol={subvol}")
                    } else {
                        format!("root=UUID={uuid}")
                    }
                }
                superblock::Kind::Luks2 => {
                    let uuid = self.uuid.as_ref().expect("cannot have luks2 without uuid");
                    format!("rd.luks.uuid={uuid}")
                }
                _ => {
                    if let Some(guid) = self.guid.as_ref() {
                        format!("root=PARTUUID={guid}")
                    } else if let Some(uuid) = self.uuid.as_ref() {
                        format!("root=UUID={uuid}")
                    } else {
                        String::new()
                    }
                }
            }
        } else if !self.aux {
            format!("root={}", self.path)
        } else {
            String::new()
        };

        format!("{local} {children}").trim().to_owned()
    }
}
