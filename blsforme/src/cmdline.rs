use std::{io, path::Path};

use fs_err as fs;
use itertools::Itertools;
use topology::disk::device::BlockDevice;

use crate::{
    Configuration, Entry, Kernel,
    file_utils::{PathExt, read_dir_iter},
};

/// Global cmdline used as the basis for all boot entries.
///
/// This includes the `root=` related cmdline detected from
/// the system's root [`BlockDevice`] and all local snippets
/// loaded from `/etc`, including exclusions which will be
/// used to filter out kernel specific cmdline.
///
/// This is used per [`Entry`] to form the final cmdline
/// based on the loaded snippets for that entries kernel.
#[derive(Debug, Default)]
pub struct GlobalCmdline {
    /// Cmdline containing root device.
    root: [String; 2],
    /// Cmdline snippets loaded from `/etc`.
    etc_snippets: Vec<String>,
    /// Cmdline exclusions loaded from `/etc`.
    etc_exclusions: Vec<String>,
}

impl GlobalCmdline {
    /// Returns a [`GlobalCmdline`] using the supplied [`BlockDevice`] for `root=`
    /// and loading all snippets & exclusions from `/etc`.
    pub fn new(config: &Configuration, root: &BlockDevice<'_>) -> Self {
        log::info!("root = {:?}", root.cmd_line());

        let (etc_snippets, etc_exclusions) = load_etc_cmdline(config);

        Self {
            root: [root.cmd_line(), "rw".to_owned()],
            etc_snippets,
            etc_exclusions,
        }
    }

    /// The global cmdline string of this [`GlobalCmdline`].
    pub fn cmdline(&self) -> String {
        self.root.iter().chain(&self.etc_snippets).join(" ")
    }

    /// Merge all detected snippets into a full cmdline string for the supplied [`Entry`].
    ///
    /// Ordering:
    ///
    /// - root cmdline (root=)
    /// - kernel cmdline w/out exclusions
    /// - manually defined [`Entry`] cmdline
    /// - /etc cmdline
    pub fn full_cmdline(&self, entry: &Entry) -> String {
        let kernel_cmdline = load_kernel_cmdline(&entry.sysroot, entry.kernel, &self.etc_exclusions);

        self.root
            .iter()
            .chain(&kernel_cmdline)
            .chain(&entry.cmdline)
            .chain(&self.etc_snippets)
            .join(" ")
    }
}

/// Read a cmdline snippet from a file, which supports comments (`#`)
/// and concatenates lines into a single string.
fn cmdline_snippet(path: impl AsRef<Path>) -> io::Result<String> {
    let path = path.as_ref();
    log::trace!("Reading cmdline snippet: {path:?}");
    let ret = fs::read_to_string(path)?
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join(" ")
        .to_string();
    Ok(ret)
}

/// Loads all cmdline & exclusion snippets from `/etc`.
fn load_etc_cmdline(config: &Configuration) -> (Vec<String>, Vec<String>) {
    let mut snippets = vec![];
    let mut exclusions = vec![];

    let etc_cmdline_d = config.root.path().join("etc").join("kernel").join("cmdline.d");

    let etc_entries = read_dir_iter(&etc_cmdline_d)
        .flat_map(|entry| {
            let path = entry.path();
            path.extension().is_some_and(|e| e == "cmdline").then_some(path)
        })
        .collect::<Vec<_>>();

    for entry in etc_entries {
        // For anything that's a symlink to /dev/null, we'll exclude the matching system-wide cmdline
        if entry.is_symlink()
            && let Some(file_name) = entry.file_name_str()
        {
            if let Ok(target) = entry.read_link() {
                if target.as_path() == Path::new("/dev/null") {
                    log::trace!("excluding system-wide cmdline.d entry {entry:?}");
                    exclusions.push(file_name.to_owned());
                    continue;
                }
            }
        }
        if let Ok(c) = cmdline_snippet(entry) {
            snippets.push(c);
        }
    }

    (snippets, exclusions)
}

fn load_kernel_cmdline(sysroot: &Path, kernel: &Kernel, exclusions: &[String]) -> Vec<String> {
    let mut snippets = vec![];

    // Load local cmdline snippets for this kernel entry
    for snippet in kernel
        .extras
        .iter()
        .filter(|e| matches!(e.kind, crate::AuxiliaryKind::Cmdline))
    {
        let Some(file_name) = snippet.path.file_name_str() else {
            continue;
        };

        // Only add if its not excluded
        if !exclusions.contains(&file_name.to_owned()) {
            snippets.extend(cmdline_snippet(sysroot.join(&snippet.path)));
        }
    }

    // Globals
    let cmdline_d = sysroot.join("usr").join("lib").join("kernel").join("cmdline.d");

    if !cmdline_d.exists() {
        return snippets;
    }

    for entry in read_dir_iter(&cmdline_d) {
        let path = entry.path();
        let Some(file_name) = path.file_name_str() else {
            continue;
        };

        // Only add if its not excluded
        if !exclusions.contains(&file_name.to_owned()) {
            snippets.extend(cmdline_snippet(path));
        }
    }

    snippets
}
