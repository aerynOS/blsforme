// SPDX-FileCopyrightText: Copyright © 2025 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

use std::path::PathBuf;

use crate::{AuxiliaryFile, Configuration, Kernel, Schema};

/// An entry corresponds to a single kernel, and may have a supplemental
/// cmdline
#[derive(Debug)]
pub struct Entry<'a> {
    pub(crate) kernel: &'a Kernel,

    pub(crate) sysroot: PathBuf,

    pub(crate) cmdline: Vec<String>,

    /// Unique state ID for this entry
    pub(crate) state_id: Option<i32>,

    /// Entry-specific schema for overriding the global schema
    pub(crate) schema: Option<Schema>,
}

impl<'a> Entry<'a> {
    /// New entry for the given kernel
    pub fn new(config: &Configuration, kernel: &'a Kernel) -> Self {
        Self {
            kernel,
            sysroot: config.root.path().to_owned(),
            cmdline: vec![],
            state_id: None,
            schema: None,
        }
    }

    /// With the given system root
    /// This affects where local snippets & files are loaded from
    pub fn with_sysroot(self, sysroot: impl Into<PathBuf>) -> Self {
        Self {
            sysroot: sysroot.into(),
            ..self
        }
    }

    /// With the given state ID
    /// Used by moss to link to the unique transaction ID on disk
    pub fn with_state_id(self, state_id: i32) -> Self {
        Self {
            state_id: Some(state_id),
            ..self
        }
    }

    /// With the given schema
    /// Used by moss to override the global schema
    pub fn with_schema(self, schema: Schema) -> Self {
        Self {
            schema: Some(schema),
            ..self
        }
    }

    /// With the given cmdline entry
    /// Used by moss to inject a `moss.tx={}` parameter
    pub fn with_cmdline(self, entry: String) -> Self {
        let mut cmdline = self.cmdline;
        cmdline.push(entry);
        Self { cmdline, ..self }
    }

    /// Return an entry ID, suitable for `.conf` generation
    pub fn id(&self, schema: &Schema) -> String {
        // Prefer internal schema if available
        let effective_schema = self.schema.as_ref().unwrap_or(schema);

        let id = match effective_schema {
            Schema::Legacy { os_release, .. } => os_release.name.clone(),
            _ => effective_schema.os_id(),
        };
        if let Some(state_id) = self.state_id.as_ref() {
            format!("{id}-{version}-{state_id}", version = self.kernel.version)
        } else {
            format!("{id}-{version}", version = self.kernel.version)
        }
    }

    /// Generate an installed name for the kernel, used by bootloaders
    /// Right now this only returns CBM style IDs
    pub fn installed_kernel_name(&self, schema: &Schema) -> Option<String> {
        // Prefer internal schema if available
        let effective_schema = self.schema.as_ref().unwrap_or(schema);

        match effective_schema {
            Schema::Legacy { .. } => self
                .kernel
                .image
                .file_name()
                .map(|f| f.to_string_lossy())
                .map(|filename| format!("kernel-{filename}")),
            _ => Some(format!("{}/vmlinuz", self.kernel.version)),
        }
    }

    /// Generate installed asset (aux) name, used by bootloaders
    /// Right now this only returns CBM style IDs
    pub fn installed_asset_name(&self, schema: &Schema, asset: &AuxiliaryFile) -> Option<String> {
        // Prefer internal schema if available
        let effective_schema = self.schema.as_ref().unwrap_or(schema);

        match effective_schema {
            Schema::Legacy { .. } => match asset.kind {
                crate::AuxiliaryKind::InitRd => asset
                    .path
                    .file_name()
                    .map(|f| f.to_string_lossy())
                    .map(|filename| format!("initrd-{filename}")),
                _ => None,
            },
            _ => {
                let filename = asset.path.file_name().map(|f| f.to_string_lossy())?;
                match asset.kind {
                    crate::AuxiliaryKind::InitRd => Some(format!("{}/{}", self.kernel.version, filename)),
                    _ => None,
                }
            }
        }
    }
}
