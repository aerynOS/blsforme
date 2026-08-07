// SPDX-FileCopyrightText: Copyright © 2025 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

//! systemd-boot management and interfaces

use std::path::PathBuf;

use fs_err as fs;
use snafu::{OptionExt as _, ResultExt as _};

use crate::{
    Entry, GlobalCmdline, Kernel, Schema,
    bootloader::{IoSnafu, MissingFileSnafu, MissingMountSnafu, PrefixSnafu},
    file_utils::{PathExt, changed_files, copy_atomic_vfat},
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
            copy_atomic_vfat(source, dest).context(IoSnafu)?;
        }

        // Write the loader.conf file with default entry pattern based on namespace
        let loader_conf_dir = self.boot_root.join_insensitive("loader");
        let loader_conf_path = loader_conf_dir.join_insensitive("loader.conf");
        if !loader_conf_dir.exists() {
            fs::create_dir_all(loader_conf_dir).context(IoSnafu)?;
        }

        // Create a default pattern that matches all entries for our namespace
        let namespace = self.schema.os_namespace();
        let default_pattern = format!("default \"{namespace}*\"\n");
        fs::write(loader_conf_path, default_pattern).context(IoSnafu)?;

        Ok(())
    }

    pub(super) fn sync_entries(&self, cmdline: &GlobalCmdline, entries: &[Entry]) -> Result<(), super::Error> {
        let mut installed_entries = vec![];
        for entry in entries {
            let full_cmdline = cmdline.full_cmdline(entry);
            let installed = self.install(&full_cmdline, entry)?;
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
        if let Ok(entries) = fs::read_dir(&loader_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let file_name = entry.file_name().to_string_lossy().to_string();
                if all_prefixes.iter().any(|prefix| file_name.starts_with(prefix)) {
                    loader_files.push(entry.path());
                }
            }
        }

        // Check each namespace for kernel directories
        let mut kernel_dirs = Vec::new();
        for namespace in &all_namespaces {
            let efi_dir = self.boot_root.join_insensitive("EFI").join_insensitive(namespace);
            if efi_dir.exists() {
                if let Ok(entries) = fs::read_dir(&efi_dir) {
                    for entry in entries.filter_map(|e| e.ok()) {
                        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                            kernel_dirs.push(entry.path());
                        }
                    }
                }
            }
        }

        let obsolete_loader_confs = loader_files
            .iter()
            .filter(|f| !installed_entries.iter().any(|e| e.loader_conf == f.to_string_lossy()))
            .collect::<Vec<_>>();

        let obsolete_kernels = kernel_dirs
            .iter()
            .filter(|f| !installed_entries.iter().any(|e| e.kernel_dir == f.to_string_lossy()))
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

        Ok(())
    }

    /// Install a kernel to the ESP or XBOOTLDR, write a config for it
    fn install(&self, cmdline: &str, entry: &Entry) -> Result<InstallResult, super::Error> {
        let effective_schema = entry.schema.as_ref().unwrap_or(self.schema);

        let loader_id = self
            .boot_root
            .join_insensitive("loader")
            .join_insensitive("entries")
            .join_insensitive(format!("{}.conf", entry.id(effective_schema)));
        log::trace!("writing entry: {}", loader_id.display());

        let sysroot = &entry.sysroot;

        // Get kernel directory for this specific entry
        let kernel_dir = self.get_kernel_dir(entry);

        // vmlinuz primary path
        let vmlinuz = kernel_dir.join_insensitive(
            entry
                .installed_kernel_name(effective_schema)
                .context(MissingFileSnafu { filename: "vmlinuz" })?,
        );
        // initrds requiring install
        let initrds = entry
            .kernel
            .initrd
            .iter()
            .filter_map(|asset| {
                Some((
                    sysroot.join(&asset.path),
                    kernel_dir.join_insensitive(entry.installed_asset_name(effective_schema, asset)?),
                ))
            })
            .collect::<Vec<_>>();
        log::trace!("with kernel path: {}", vmlinuz.display());
        log::trace!("with initrds: {initrds:?}");

        // build up the total changeset
        let mut changeset = vec![(sysroot.join(&entry.kernel.image), vmlinuz.clone())];
        changeset.extend(initrds);

        // Determine which need copying now.
        let needs_writing = changed_files(changeset.as_slice());
        log::trace!("requires update: {needs_writing:?}");

        // Donate them to disk
        for (source, dest) in needs_writing {
            copy_atomic_vfat(source, dest).context(IoSnafu)?;
        }

        let asset_dir = kernel_dir
            .strip_prefix(&self.boot_root)
            .context(PrefixSnafu)?
            .to_string_lossy();

        let loader_config = self.generate_entry(&asset_dir, cmdline, entry);
        log::trace!("loader config: {loader_config}");

        let entry_dir = self.boot_root.join_insensitive("loader").join_insensitive("entries");
        if !entry_dir.exists() {
            fs::create_dir_all(entry_dir).context(IoSnafu)?;
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
        };

        // TODO: Hash compare and dont obliterate!
        fs::write(loader_id, loader_config).context(IoSnafu)?;

        Ok(tracker)
    }

    /// Generate a usable loader config entry
    fn generate_entry(&self, asset_dir: &str, cmdline: &str, entry: &Entry) -> String {
        let effective_schema = entry.schema.as_ref().unwrap_or(self.schema);

        let initrd = if entry.kernel.initrd.is_empty() {
            "\n".to_string()
        } else {
            let initrds = entry
                .kernel
                .initrd
                .iter()
                .filter_map(|asset| {
                    Some(format!(
                        "\ninitrd /{asset_dir}/{}",
                        entry.installed_asset_name(effective_schema, asset)?
                    ))
                })
                .collect::<String>();
            format!("\n{initrds}")
        };
        let title = if let Some(pretty) = effective_schema.os_display_name() {
            format!("{pretty} ({})", entry.kernel.version)
        } else {
            format!("{} ({})", effective_schema.os_name(), entry.kernel.version)
        };
        let vmlinuz = entry.installed_kernel_name(effective_schema).expect("linux go boom");
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

        for entry in fs::read_dir(&base_kernel_dir).context(IoSnafu)? {
            let entry = entry.context(IoSnafu)?;
            if !entry.file_type().context(IoSnafu)?.is_dir() {
                continue;
            }
            let paths = fs::read_dir(entry.path())
                .context(IoSnafu)?
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
