// SPDX-FileCopyrightText: Copyright © 2025 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

//! Kernel abstraction

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::{Error, file_utils::PathExt, os_release::OsRelease};
use os_info::OsInfo;

/// Control kernel discovery mechanism
#[derive(Debug)]
pub enum Schema {
    /// Legacy (clr-boot-manager style) schema
    Legacy {
        os_release: Box<OsRelease>,
        namespace: &'static str,
    },

    /// Legacy, using only an os-release file
    Blsforme { os_release: Box<OsRelease> },

    /// Modern distribution using os-info.json
    OsInfo { os_info: Box<OsInfo> },
}

/// `boot.json` deserialise support
#[derive(Deserialize)]
pub struct BootJSON<'a> {
    /// Kernel's package name
    #[serde(borrow)]
    pub name: &'a str,

    /// Kernel's version string (uname -r)
    #[serde(borrow)]
    pub version: &'a str,

    /// Kernel's variant id
    #[serde(borrow)]
    pub variant: &'a str,
}

impl<'a> TryFrom<&'a str> for BootJSON<'a> {
    type Error = serde_json::Error;

    fn try_from(value: &'a str) -> Result<Self, Self::Error> {
        serde_json::from_str::<Self>(value)
    }
}

/// A kernel is the primary bootable element that we care about, ie
/// the vmlinuz file. It also comes with a set of auxiliary files
/// that are required for a fully working system, but specifically
/// dependent on that kernel version.
#[derive(Debug, PartialEq, PartialOrd, Eq, Ord)]
pub struct Kernel {
    /// Matches the `uname -r` of the kernel, should be uniquely encoded by release/variant
    pub version: String,

    /// vmlinuz path
    pub image: PathBuf,

    /// All of the initrds
    pub initrd: Vec<AuxiliaryFile>,

    /// Any non-initrd, auxiliary files
    pub extras: Vec<AuxiliaryFile>,

    /// Recorded variant type
    pub variant: Option<String>,
}

/// Denotes the kind of auxiliary file
#[derive(Debug, PartialEq, PartialOrd, Eq, Ord)]
pub enum AuxiliaryKind {
    /// A cmdline snippet
    Cmdline,

    /// A versioned initial ramdisk
    VersionedInitRd,

    /// A shared initial ramdisk
    SharedInitRd,

    /// System.map file
    SystemMap,

    /// .config file
    Config,

    /// The `boot.json` file
    BootJson,
}

/// An additional file required to be shipped with the kernel,
/// such as initrds, system maps, etc.
#[derive(Debug, PartialEq, PartialOrd, Eq, Ord)]
pub struct AuxiliaryFile {
    pub path: PathBuf,
    pub kind: AuxiliaryKind,
}

impl Schema {
    /// Given a set of kernel-like paths, yield all potential kernels within them
    /// This should be a set of `/usr/lib/kernel` paths. Use glob or appropriate to discover.
    pub fn discover_system_kernels(&self, paths: impl Iterator<Item = impl AsRef<Path>>) -> Result<Vec<Kernel>, Error> {
        match &self {
            Schema::Legacy { namespace, .. } => Self::legacy_kernels(namespace, paths),
            Schema::Blsforme { .. } => Self::blsforme_kernels(paths),
            Schema::OsInfo { .. } => Self::blsforme_kernels(paths),
        }
    }

    /// Retrieve the OS name
    pub fn os_name(&self) -> String {
        match self {
            Schema::Legacy { os_release, .. } => os_release.name.clone(),
            Schema::Blsforme { os_release } => os_release.name.clone(),
            Schema::OsInfo { os_info } => os_info.metadata.identity.name.clone(),
        }
        .to_string()
    }

    /// Retrieve the namespace for files on the boot partition(s)
    pub fn os_namespace(&self) -> String {
        match self {
            Schema::Legacy { namespace, .. } => namespace.to_string(),
            Schema::Blsforme { os_release } => os_release.id.clone(),
            Schema::OsInfo { os_info } => os_info.metadata.identity.id.clone(),
        }
    }

    /// Retrieve the OS ID (ie `serpent-os`, `aerynos`, etc)
    /// This is the `ID` field in os-release
    pub fn os_id(&self) -> String {
        match self {
            Schema::Legacy { os_release, .. } => os_release.id.clone(),
            Schema::Blsforme { os_release } => os_release.id.clone(),
            Schema::OsInfo { os_info } => os_info.metadata.identity.id.clone(),
        }
        .to_string()
    }

    /// Retrieve display name for the OS
    /// This is the `PRETTY_NAME` field in os-release, used for display purposes
    pub fn os_display_name(&self) -> Option<String> {
        match self {
            Schema::Legacy { os_release, .. } => os_release.meta.pretty_name.clone(),
            Schema::Blsforme { os_release } => os_release.meta.pretty_name.clone(),
            Schema::OsInfo { os_info } => Some(os_info.metadata.identity.display.clone()),
        }
    }

    /// Discover any legacy kernels
    fn legacy_kernels(
        namespace: &'static str,
        paths: impl Iterator<Item = impl AsRef<Path>>,
    ) -> Result<Vec<Kernel>, Error> {
        let paths = paths.collect::<Vec<_>>();
        // First up, find kernels. They start with the prefix..
        let candidates = paths
            .iter()
            .filter_map(|p| p.as_ref().file_name()?.to_str()?.starts_with(namespace).then_some(p));

        let mut kernels = BTreeMap::new();

        // TODO: Make use of release
        for cand in candidates {
            let item = cand.as_ref();
            if let Some(file_name) = item.file_name().map(|f| f.to_string_lossy().to_string()) {
                let (left, right) = file_name.split_at(namespace.len() + 1);
                assert!(left.ends_with('.'));
                if let Some((variant, full_version)) = right.split_once('.') {
                    if let Some((_version, _release)) = full_version.rfind('-').map(|i| full_version.split_at(i)) {
                        log::trace!("discovered vmlinuz: {file_name}");
                        kernels.insert(
                            full_version.to_string(),
                            Kernel {
                                version: full_version.to_string(),
                                image: item.into(),
                                initrd: vec![],
                                extras: vec![],
                                variant: Some(variant.to_string()),
                            },
                        );
                    }
                }
            }
        }

        // Find all the AUX files
        for (version, kernel) in kernels.iter_mut() {
            let variant_str = kernel.variant.as_ref().map(|v| format!(".{v}")).unwrap_or_default();
            let sysmap_file = format!("System.map-{version}{variant_str}");
            let cmdline_file = format!("cmdline-{version}{variant_str}");
            let config_file = format!("config-{version}{variant_str}");
            let indep_initrd = format!("initrd-{namespace}.");
            let initrd_file = format!(
                "initrd-{}{}{}",
                namespace,
                kernel.variant.as_ref().map(|v| format!(".{v}.")).unwrap_or_default(),
                version
            );

            for path in paths.iter() {
                let filename = path
                    .as_ref()
                    .file_name()
                    .ok_or(Error::InvalidFilesystem)?
                    .to_str()
                    .ok_or(Error::InvalidFilesystem)?;

                let aux = match filename {
                    x if x == sysmap_file => Some(AuxiliaryFile {
                        path: path.as_ref().into(),
                        kind: AuxiliaryKind::SystemMap,
                    }),
                    x if x == cmdline_file => Some(AuxiliaryFile {
                        path: path.as_ref().into(),
                        kind: AuxiliaryKind::Cmdline,
                    }),
                    x if x == config_file => Some(AuxiliaryFile {
                        path: path.as_ref().into(),
                        kind: AuxiliaryKind::Config,
                    }),
                    x if x == initrd_file => Some(AuxiliaryFile {
                        path: path.as_ref().into(),
                        kind: AuxiliaryKind::VersionedInitRd,
                    }),
                    x if x.starts_with(&initrd_file) => {
                        // Version dependent initrd
                        if x != initrd_file && x.split_once(&initrd_file).is_some() {
                            Some(AuxiliaryFile {
                                path: path.as_ref().into(),
                                kind: AuxiliaryKind::VersionedInitRd,
                            })
                        } else {
                            None
                        }
                    }
                    x if x.starts_with(&indep_initrd) => {
                        // Version independent initrd
                        if let Some((_, r)) = x.split_once(&indep_initrd) {
                            if !r.contains('.') {
                                Some(AuxiliaryFile {
                                    path: path.as_ref().into(),
                                    kind: AuxiliaryKind::SharedInitRd,
                                })
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    _ => None,
                };

                if let Some(aux_file) = aux {
                    if matches!(
                        aux_file.kind,
                        AuxiliaryKind::VersionedInitRd | AuxiliaryKind::SharedInitRd
                    ) {
                        kernel.initrd.push(aux_file);
                    } else {
                        kernel.extras.push(aux_file);
                    }
                }
            }

            kernel
                .initrd
                .sort_by_key(|i| i.path.display().to_string().to_lowercase());
            kernel
                .extras
                .sort_by_key(|e| e.path.display().to_string().to_lowercase());
        }
        Ok(kernels.into_values().collect::<Vec<_>>())
    }

    // Handle newstyle discovery
    fn blsforme_kernels(paths: impl Iterator<Item = impl AsRef<Path>>) -> Result<Vec<Kernel>, Error> {
        let all_paths = paths.map(|m| m.as_ref().to_path_buf()).collect::<BTreeSet<_>>();

        // all `vmlinuz` files within the set
        let mut kernel_images = all_paths
            .iter()
            .filter(|p| p.ends_with("vmlinuz"))
            .filter_map(|m| {
                let version = m.parent()?.file_name()?.to_str()?.to_string();
                Some((
                    version.clone(),
                    Kernel {
                        version,
                        image: PathBuf::from(m),
                        initrd: vec![],
                        extras: vec![],
                        variant: None,
                    },
                ))
            })
            .collect::<HashMap<_, _>>();

        // Walk kernels, find matching assets
        for (version, kernel) in kernel_images.iter_mut() {
            let kernel_dir = kernel.image.parent().ok_or(Error::InvalidFilesystem)?;
            let versioned_assets = all_paths
                .iter()
                .filter(|p| !p.ends_with("vmlinuz") && p.starts_with(kernel_dir) && !p.ends_with(version));
            for asset in versioned_assets {
                let filename = asset
                    .file_name()
                    .ok_or(Error::InvalidFilesystem)?
                    .to_str()
                    .ok_or(Error::InvalidFilesystem)?;
                let aux = match filename {
                    "System.map" => Some(AuxiliaryFile {
                        path: asset.clone(),
                        kind: AuxiliaryKind::SystemMap,
                    }),
                    "boot.json" => Some(AuxiliaryFile {
                        path: asset.clone(),
                        kind: AuxiliaryKind::BootJson,
                    }),
                    "config" => Some(AuxiliaryFile {
                        path: asset.clone(),
                        kind: AuxiliaryKind::Config,
                    }),
                    // Older paradigm used before we had proper shared initrd support.
                    // `-shared.initrd` files were placed beneath the versioned kernel
                    // directory so they'd get picked up, but not deduplicated.
                    _ if filename.ends_with("-shared.initrd") => Some(AuxiliaryFile {
                        path: asset.clone(),
                        kind: AuxiliaryKind::SharedInitRd,
                    }),
                    _ if filename.ends_with(".initrd") => Some(AuxiliaryFile {
                        path: asset.clone(),
                        kind: AuxiliaryKind::VersionedInitRd,
                    }),
                    _ if filename.ends_with(".cmdline") => Some(AuxiliaryFile {
                        path: asset.clone(),
                        kind: AuxiliaryKind::Cmdline,
                    }),
                    _ => None,
                };

                if let Some(aux_file) = aux {
                    if matches!(
                        aux_file.kind,
                        AuxiliaryKind::VersionedInitRd | AuxiliaryKind::SharedInitRd
                    ) {
                        kernel.initrd.push(aux_file);
                    } else {
                        kernel.extras.push(aux_file);
                    }
                }
            }

            // Add all shared init.rd files from `initrd.d`. Due to how we used to ship
            // shared initrd files under the versioned kernel dir, anything here should
            // "override" a matching file found from that dir. It existing here is
            // hard confirmation this is a shared initrd filename.
            let Some(initrd_d) = kernel_dir.parent().map(|p| p.join("initrd.d")) else {
                continue;
            };
            let shared_initrd_assets = all_paths
                .iter()
                .filter(|p| p.starts_with(&initrd_d) && p.file_name_str().unwrap_or_default().ends_with(".initrd"));
            for asset in shared_initrd_assets {
                let filename = asset
                    .file_name()
                    .ok_or(Error::InvalidFilesystem)?
                    .to_str()
                    .ok_or(Error::InvalidFilesystem)?;

                // If a shared initrd collides w/ an initrd found in the versioned kernel
                // dir, remove the versioned initrd since those were the old hacky way
                // of shipping shared initrd & we don't want to add them twice.
                kernel
                    .initrd
                    .retain(|file| file.path.file_name_str().unwrap_or_default() != filename);

                kernel.initrd.push(AuxiliaryFile {
                    path: asset.to_owned(),
                    kind: AuxiliaryKind::SharedInitRd,
                });
            }

            // Sort by file name, not path, since assets can exist between the
            // `shared` directory and the versioned kernel directory. The filename
            // should have the `NN-` prefix for determining the order for how
            // assets should be loaded.
            kernel
                .initrd
                .sort_by_key(|i| i.path.file_name_str().unwrap_or_default().to_lowercase());
            kernel
                .extras
                .sort_by_key(|e| e.path.file_name_str().unwrap_or_default().to_lowercase());
        }

        Ok(kernel_images.into_values().collect::<Vec<_>>())
    }
}

#[cfg(test)]
mod tests {
    use fs_err as fs;

    use super::BootJSON;

    #[test]
    fn test_boot_json() {
        let text = fs::read_to_string("boot.json").expect("Failed to read json file");
        let boot = BootJSON::try_from(text.as_str()).expect("Failed to decode JSON");
        assert_eq!(boot.name, "linux-desktop");
        assert_eq!(boot.variant, "desktop");
        assert_eq!(boot.version, "6.8.2-25.desktop");
    }
}
