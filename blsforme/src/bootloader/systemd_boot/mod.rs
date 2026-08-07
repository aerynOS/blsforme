// SPDX-FileCopyrightText: Copyright © 2025 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

//! systemd-boot management and interfaces

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use fs_err as fs;
use snafu::{OptionExt as _, ResultExt as _};

use crate::{
    AuxiliaryKind, Entry, GlobalCmdline, Kernel, Schema,
    bootloader::{IoSnafu, MissingFileSnafu, MissingFinalComponentSnafu, MissingMountSnafu, PrefixSnafu},
    file_utils::{PathExt, changed_files, copy_atomic_vfat, read_dir_iter},
    manager::Mounts,
};

pub mod interface;

/// systemd specific bootloader behaviours
/// NOTE: Currently secure boot is NOT supported (or fbx64)
#[derive(Debug)]
pub struct Loader<'a, 'b> {
    /// system configuration
    #[allow(dead_code)]
    assets: &'b [PathBuf],
    mounts: &'a Mounts,

    schema: &'a Schema,
    boot_root: PathBuf,
}

#[derive(Debug)]
struct InstallResult {
    /// The `.conf` file that was written (absolute)
    loader_conf: String,

    // The kernel path that was installed (absolute)
    kernel_dir: String,

    // The image & initrd files installed under `kernel_dir` (absolute)
    kernel_files: Vec<String>,
}

impl<'a, 'b> Loader<'a, 'b> {
    /// Construct a new systemd boot loader manager
    pub(super) fn new(schema: &'a Schema, assets: &'b [PathBuf], mounts: &'a Mounts) -> Result<Self, super::Error> {
        let boot_root = mounts
            .xbootldr
            .clone()
            .or_else(|| mounts.esp.clone())
            .context(MissingMountSnafu {
                description: "ESP (/efi)",
            })?;

        Ok(Self {
            schema,
            assets,
            mounts,
            boot_root,
        })
    }

    /// Get the kernel directory for a specific entry
    fn get_kernel_dir(&self, entry: &Entry) -> PathBuf {
        let effective_schema = entry.schema.as_ref().unwrap_or(self.schema);
        self.boot_root
            .join_insensitive("EFI")
            .join_insensitive(effective_schema.os_namespace())
    }

    /// Sync bootloader to ESP (not XBOOTLDR..)
    pub(super) fn sync(&self) -> Result<(), super::Error> {
        let x64_efi = self
            .assets
            .iter()
            .find(|p| p.ends_with("systemd-bootx64.efi"))
            .context(MissingFileSnafu {
                filename: "systemd-bootx64.efi",
            })?;
        log::debug!("discovered main efi asset: {}", x64_efi.display());

        let esp = self.mounts.esp.as_ref().context(MissingMountSnafu {
            description: "ESP (/efi)",
        })?;
        // Copy systemd-bootx64.efi into these locations
        let targets = vec![
            (
                x64_efi.clone(),
                esp.join_insensitive("EFI")
                    .join_insensitive("Boot")
                    .join_insensitive("BOOTX64.EFI"),
            ),
            (
                x64_efi.clone(),
                esp.join_insensitive("EFI")
                    .join_insensitive("systemd")
                    .join_insensitive("systemd-bootx64.efi"),
            ),
        ];

        for (source, dest) in changed_files(targets.as_slice()) {
            copy_atomic_vfat(source, dest).context(IoSnafu {
                context: "copy changed files",
            })?;
        }

        // Write the loader.conf file with default entry pattern based on namespace
        let loader_conf_dir = self.boot_root.join_insensitive("loader");
        let loader_conf_path = loader_conf_dir.join_insensitive("loader.conf");
        if !loader_conf_dir.exists() {
            fs::create_dir_all(loader_conf_dir).context(IoSnafu {
                context: "create loader conf dir",
            })?;
        }

        // Create a default pattern that matches all entries for our namespace
        let namespace = self.schema.os_namespace();
        let default_pattern = format!("default \"{namespace}*\"\n");
        fs::write(loader_conf_path, default_pattern).context(IoSnafu {
            context: "write loader.conf",
        })?;

        Ok(())
    }

    pub(super) fn sync_entries(&self, cmdline: &GlobalCmdline, entries: &[Entry]) -> Result<(), super::Error> {
        let mut hasher = blake3::Hasher::new();
        let mut installed_entries = vec![];

        // Hash all assets installed by these entries
        let hashed_entries = entries
            .iter()
            .map(|entry| EntryWithHashedAssets::new(&mut hasher, entry))
            .collect::<Result<Vec<_>, _>>()?;

        // Compute the collisions between all entry assets so we can
        // produce conflict-aware file names.
        let collisions = AssetCollisions::from_entries(&hashed_entries);

        for entry in &hashed_entries {
            let installed = self.install(cmdline, &collisions, entry)?;
            installed_entries.push(installed);
        }

        self.cleanup_stale_entries(&installed_entries)?;

        Ok(())
    }

    /// Clean up stale loader configs and kernel directories
    fn cleanup_stale_entries(&self, installed_entries: &[InstallResult]) -> Result<(), super::Error> {
        let all_namespaces = match self.schema {
            Schema::OsInfo { os_info } => {
                // Include all former identities
                let mut old_ids = os_info
                    .metadata
                    .identity
                    .former_identities
                    .iter()
                    .map(|i| i.id.clone())
                    .collect::<Vec<_>>();
                old_ids.push(os_info.metadata.identity.id.clone());
                old_ids
            }
            _ => vec![self.schema.os_namespace()],
        };

        let all_prefixes = match self.schema {
            Schema::OsInfo { os_info } => {
                // Include all former identities
                let mut old_ids = os_info
                    .metadata
                    .identity
                    .former_identities
                    .iter()
                    .map(|i| i.id.clone())
                    .collect::<Vec<_>>();
                old_ids.push(os_info.metadata.identity.id.clone());
                old_ids
            }
            Schema::Legacy { os_release, .. } => vec![os_release.name.clone()],
            _ => vec![self.schema.os_id()],
        };

        let loader_dir = self.boot_root.join_insensitive("loader").join_insensitive("entries");

        // Find all loader files that match any of our prefixes
        let mut loader_files = Vec::new();
        for entry in read_dir_iter(&loader_dir) {
            let file_name = entry.file_name().to_string_lossy().to_string();
            if all_prefixes.iter().any(|prefix| file_name.starts_with(prefix)) {
                loader_files.push(entry.path());
            }
        }

        // Check each namespace for kernel directories
        let mut kernel_files = HashMap::new();
        for namespace in &all_namespaces {
            let efi_dir = self.boot_root.join_insensitive("EFI").join_insensitive(namespace);

            if !efi_dir.exists() {
                continue;
            }

            for entry in read_dir_iter(&efi_dir) {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    kernel_files.insert(entry.path(), vec![]);
                }
            }
        }
        // Add all files below each kernel directory
        for (kernel_dir, files) in kernel_files.iter_mut() {
            for entry in read_dir_iter(kernel_dir) {
                if entry.file_type().is_ok_and(|t| t.is_file()) {
                    files.push(entry.path());
                }
            }
        }

        let obsolete_loader_confs = loader_files
            .iter()
            .filter(|f| !installed_entries.iter().any(|e| e.loader_conf == f.to_string_lossy()))
            .collect::<Vec<_>>();

        let obsolete_kernels = kernel_files
            .keys()
            .filter(|dir| {
                // Don't cleanup shared dir if used by this scheme
                if dir.file_name_str().unwrap_or_default() == "shared"
                    && matches!(self.schema, Schema::Blsforme { .. } | Schema::OsInfo { .. })
                {
                    return false;
                }

                // Remove kernel dirs that have no more matching entries
                !installed_entries.iter().any(|e| e.kernel_dir == dir.to_string_lossy())
            })
            .collect::<Vec<_>>();

        let obsolete_kernel_files = kernel_files
            .iter()
            .flat_map(|(dir, files)| {
                let obsolete_kernels = &obsolete_kernels;
                files.iter().filter(move |file| {
                    // Already removing the entire dir
                    !obsolete_kernels.contains(&dir)
                        && !installed_entries
                            .iter()
                            .any(|e| e.kernel_files.contains(&file.to_string_lossy().to_string()))
                })
            })
            .collect::<Vec<_>>();

        for conf in obsolete_loader_confs.iter() {
            log::info!("Removing stale loader config: {conf:?}");
            if let Err(e) = fs::remove_file(conf) {
                log::error!("Failed to remove stale loader config {conf:?}: {e}")
            }
        }

        for tree in obsolete_kernels.iter() {
            log::info!("Removing stale kernel tree: {tree:?}");
            if let Err(e) = fs::remove_dir_all(tree) {
                log::error!("Failed to remove stale kernel tree {tree:?}: {e}")
            }
        }

        for file in obsolete_kernel_files.iter() {
            log::info!("Removing stale kernel file: {file:?}");
            if let Err(e) = fs::remove_file(file) {
                log::error!("Failed to remove stale kernel file {file:?}: {e}")
            }
        }

        Ok(())
    }

    /// Install a kernel to the ESP or XBOOTLDR, write a config for it
    fn install(
        &self,
        cmdline: &GlobalCmdline,
        collisions: &AssetCollisions<'_>,
        entry: &EntryWithHashedAssets<'_>,
    ) -> Result<InstallResult, super::Error> {
        let effective_schema = entry.inner.schema.as_ref().unwrap_or(self.schema);

        let loader_id = self
            .boot_root
            .join_insensitive("loader")
            .join_insensitive("entries")
            .join_insensitive(format!("{}.conf", entry.inner.id(effective_schema)));
        log::trace!("writing entry: {}", loader_id.display());

        let sysroot = &entry.inner.sysroot;

        // Get kernel directory for this specific entry
        let kernel_dir = self.get_kernel_dir(entry.inner);

        // vmlinuz primary path
        let vmlinuz = kernel_dir.join_insensitive(entry.image.installed_name(effective_schema, collisions));
        // initrds requiring install
        let initrds = entry
            .initrd
            .iter()
            .map(|initrd| {
                (
                    sysroot.join(initrd.path),
                    kernel_dir.join_insensitive(initrd.installed_name(effective_schema, collisions)),
                )
            })
            .collect::<Vec<_>>();
        log::trace!("with kernel path: {}", vmlinuz.display());
        log::trace!("with initrds: {initrds:?}");

        // build up the total changeset
        let mut changeset = vec![(sysroot.join(entry.image.path), vmlinuz.clone())];
        changeset.extend(initrds);

        // Determine which need copying now.
        let needs_writing = changed_files(changeset.as_slice());
        log::trace!("requires update: {needs_writing:?}");

        // Donate them to disk
        for (source, dest) in needs_writing {
            copy_atomic_vfat(source, dest).context(IoSnafu {
                context: "copy changed files",
            })?;
        }

        let asset_dir = kernel_dir
            .strip_prefix(&self.boot_root)
            .context(PrefixSnafu)?
            .to_string_lossy();

        let full_cmdline = cmdline.full_cmdline(entry.inner);
        let loader_config = self.generate_entry(collisions, &asset_dir, &full_cmdline, entry);
        log::trace!("loader config: {loader_config}");

        let entry_dir = self.boot_root.join_insensitive("loader").join_insensitive("entries");
        if !entry_dir.exists() {
            fs::create_dir_all(entry_dir).context(IoSnafu {
                context: "create loader entries dir",
            })?;
        }

        let tracker = InstallResult {
            loader_conf: loader_id.to_string_lossy().to_string(),
            kernel_dir: vmlinuz
                .parent()
                .context(MissingFileSnafu {
                    filename: "vmlinuz parent",
                })?
                .to_string_lossy()
                .to_string(),
            kernel_files: changeset
                .iter()
                .map(|(_, path)| path.to_string_lossy().to_string())
                .collect(),
        };

        // TODO: Hash compare and dont obliterate!
        fs::write(loader_id, loader_config).context(IoSnafu {
            context: "write entry file",
        })?;

        Ok(tracker)
    }

    /// Generate a usable loader config entry
    fn generate_entry(
        &self,
        collisions: &AssetCollisions<'_>,
        asset_dir: &str,
        cmdline: &str,
        entry: &EntryWithHashedAssets<'_>,
    ) -> String {
        let effective_schema = entry.inner.schema.as_ref().unwrap_or(self.schema);

        let initrd = if entry.initrd.is_empty() {
            "\n".to_string()
        } else {
            let initrds = entry
                .initrd
                .iter()
                .map(|asset| {
                    format!(
                        "\ninitrd /{asset_dir}/{}",
                        asset.installed_name(effective_schema, collisions)
                    )
                })
                .collect::<String>();
            format!("\n{initrds}")
        };
        let title = if let Some(pretty) = effective_schema.os_display_name() {
            format!("{pretty} ({})", entry.kernel_version())
        } else {
            format!("{} ({})", effective_schema.os_name(), entry.kernel_version())
        };
        let vmlinuz = entry.image.installed_name(effective_schema, collisions);
        format!(
            r###"title {title}
linux /{asset_dir}/{vmlinuz}{initrd}
options {cmdline}
"###
        )
    }

    pub fn installed_kernels(&self) -> Result<Vec<Kernel>, super::Error> {
        let mut all_paths = vec![];
        let base_kernel_dir = self
            .boot_root
            .join_insensitive("EFI")
            .join_insensitive(self.schema.os_namespace());

        for entry in fs::read_dir(&base_kernel_dir).context(IoSnafu {
            context: "read kernel dirs",
        })? {
            let entry = entry.context(IoSnafu {
                context: "read kernel dir entry",
            })?;
            if !entry
                .file_type()
                .context(IoSnafu {
                    context: "get kernel dir entry file type",
                })?
                .is_dir()
            {
                continue;
            }
            let paths = fs::read_dir(entry.path())
                .context(IoSnafu {
                    context: "read kernel dir entries",
                })?
                .filter_map(|p| p.ok())
                .map(|d| d.path())
                .collect::<Vec<_>>();
            all_paths.extend(paths);
        }

        if let Ok(kernels) = self.schema.discover_system_kernels(all_paths.iter()) {
            Ok(kernels)
        } else {
            Ok(vec![])
        }
    }
}

/// An [`Entry`] whose installable assets have been hashed for conflict-aware
/// installation based on not only name, but content as well.
struct EntryWithHashedAssets<'a> {
    inner: &'a Entry<'a>,
    image: HashedAsset<'a>,
    initrd: Vec<HashedAsset<'a>>,
}

impl<'a> EntryWithHashedAssets<'a> {
    fn new(hasher: &mut blake3::Hasher, entry: &'a Entry) -> Result<Self, super::Error> {
        Ok(Self {
            inner: entry,
            image: HashedAsset::new(hasher, entry, HashedAssetKind::Image, &entry.kernel.image)?,
            initrd: entry
                .kernel
                .initrd
                .iter()
                .filter_map(|file| {
                    let kind = match file.kind {
                        AuxiliaryKind::VersionedInitRd => HashedAssetKind::VersionedInitrd,
                        AuxiliaryKind::SharedInitRd => HashedAssetKind::SharedInitrd,
                        _ => return None,
                    };

                    Some(HashedAsset::new(hasher, entry, kind, &file.path))
                })
                .collect::<Result<_, _>>()?,
        })
    }

    fn kernel_version(&self) -> &'a str {
        &self.inner.kernel.version
    }
}

/// The scope determines which folder the asset
/// will be installed into, under which it must
/// have a unique filename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum AssetScope<'a> {
    /// Scoped to a kernel version
    Versioned(&'a str),
    /// Shared across any kernel
    Shared,
}

/// Unique asset key (scope + filename).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct AssetKey<'a> {
    scope: AssetScope<'a>,
    file_name: &'a str,
}

/// Tracks collisions between assets with the same [`AssetKey`],
/// but different content. This can be used to query if the installed
/// asset has a [`Self::conflict_index`], which can be used to ensure
/// the installed asset filename doesn't collide w/ the other conflicts
/// so that they all get installed instead of overwriting eachother.
#[derive(Debug, Default)]
struct AssetCollisions<'a> {
    /// Map of all unique assets & the different hashes we've seen
    /// for each (>1 == collision)
    map: HashMap<AssetKey<'a>, Vec<blake3::Hash>>,
}

impl<'a> AssetCollisions<'a> {
    /// Construct all collisions from the provided entries.
    fn from_entries(entries: &'a [EntryWithHashedAssets<'a>]) -> Self {
        let mut collisions = AssetCollisions::default();

        for entry in entries {
            collisions.insert(entry.image.key(), entry.image.blake3);

            for initrd in &entry.initrd {
                collisions.insert(initrd.key(), initrd.blake3);
            }
        }

        collisions
    }

    fn insert(&mut self, key: AssetKey<'a>, hash: blake3::Hash) {
        let hashes = self.map.entry(key).or_default();

        if !hashes.contains(&hash) {
            hashes.push(hash);
        }
    }

    /// Returns a conflict index for the supplied asset if there are
    /// collisions between this asset & others w/ the same [`AssetKey`],
    /// but different content.
    ///
    /// This is a deterministic index that can be used to differentiate
    /// this asset from its conflicts.
    fn conflict_index(&self, asset: &HashedAsset<'_>) -> Option<usize> {
        // Get all hashes seen for this asset
        let hashes = self.map.get(&asset.key()).map(Vec::as_slice).unwrap_or_default();

        // If we have conflicts (>1), get the conflict index of this hash
        hashes
            .iter()
            .position(|hash| *hash == asset.blake3)
            .filter(|_| hashes.len() > 1)
    }
}

/// Type of asset
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HashedAssetKind {
    Image,
    VersionedInitrd,
    SharedInitrd,
}

/// An asset that has been content hashed.
#[derive(Debug, Clone, Copy)]
struct HashedAsset<'a> {
    kind: HashedAssetKind,
    kernel_version: &'a str,
    path: &'a Path,
    file_name: &'a str,
    blake3: blake3::Hash,
}

impl<'a> HashedAsset<'a> {
    fn new(
        hasher: &mut blake3::Hasher,
        entry: &'a Entry<'a>,
        kind: HashedAssetKind,
        path: &'a Path,
    ) -> Result<Self, super::Error> {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context(MissingFinalComponentSnafu { path })?;
        let full_path = entry.sysroot.join(path);

        hasher.update_mmap_rayon(&full_path).context(IoSnafu {
            context: format!("hash {full_path:?}"),
        })?;
        let blake3 = hasher.finalize();
        hasher.reset();

        Ok(Self {
            kind,
            kernel_version: &entry.kernel.version,
            path,
            file_name,
            blake3,
        })
    }

    /// The unique [`AssetKey`] for this asset.
    fn key(&'a self) -> AssetKey<'a> {
        let scope = if matches!(self.kind, HashedAssetKind::SharedInitrd) {
            AssetScope::Shared
        } else {
            AssetScope::Versioned(self.kernel_version)
        };

        AssetKey {
            scope,
            file_name: self.file_name,
        }
    }

    /// Generate an installed name for the asset, used by bootloaders.
    ///
    /// Non-legacy schemes will produce a context aware name to ensure
    /// conflicting assets are both installed.
    fn installed_name(&self, schema: &Schema, collisions: &AssetCollisions) -> String {
        match schema {
            Schema::Legacy { .. } => match self.kind {
                // Need to add `kernel-`
                HashedAssetKind::Image => format!("kernel-{}", self.file_name),
                // Already has `initrd-` prefix
                HashedAssetKind::VersionedInitrd | HashedAssetKind::SharedInitrd => self.file_name.to_owned(),
            },
            _ => {
                let conflict_suffix = collisions
                    .conflict_index(self)
                    .map(|index| format!(".{index}"))
                    .unwrap_or_default();

                match self.kind {
                    HashedAssetKind::Image => {
                        format!("{}/vmlinuz{conflict_suffix}", self.kernel_version)
                    }
                    HashedAssetKind::VersionedInitrd => {
                        format!("{}/{}{conflict_suffix}", self.kernel_version, self.file_name)
                    }
                    HashedAssetKind::SharedInitrd => {
                        format!("shared/{}{conflict_suffix}", self.file_name)
                    }
                }
            }
        }
    }
}
