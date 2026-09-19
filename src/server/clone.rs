//! Fill a new server from another one on the same node.
//!
//! Copying happens here because both directories are here. Pulling a world
//! through the master and pushing it back would move gigabytes across the
//! network to land them a directory away.
//!
//! The copy is not a mirror: `.noro` stays untouched, because the new server
//! already has its own agent jar and its own tokens, and the source's would let
//! the clone speak to the master as the original.

use anyhow::{bail, Context, Result};
use std::path::Path;

use super::layout::SERVICE_DIR;

/// Directories that are a world. Prefix, not exact name: a server keeps
/// `world`, `world_nether` and `world_the_end`, and a modpack adds its own.
const WORLD_PREFIX: &str = "world";

/// Names never copied, whatever the flags say.
///
/// Logs and crash reports belong to the run that produced them — carried over,
/// they describe a crash that never happened on this server. Backups of the
/// source are its own history, and a clone that starts with somebody else's
/// restore points is a trap.
const NEVER: &[&str] = &[SERVICE_DIR, "logs", "crash-reports", "backups"];

pub struct Report {
    pub files: u64,
    pub bytes: u64,
}

/// Copy the contents of `from` into `to`.
///
/// `to` is expected to exist and be freshly installed: the clone lands on top
/// of a working server, so a failure halfway leaves something that still runs.
pub fn run(from: &Path, to: &Path, include_worlds: bool) -> Result<Report> {
    if !from.is_dir() {
        bail!("source server directory is missing");
    }
    if !to.is_dir() {
        bail!("target server directory is missing");
    }

    let mut report = Report { files: 0, bytes: 0 };
    copy_dir(from, to, include_worlds, true, &mut report)?;
    Ok(report)
}

fn copy_dir(
    from: &Path,
    to: &Path,
    include_worlds: bool,
    at_root: bool,
    report: &mut Report,
) -> Result<()> {
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if at_root && skip(&name, include_worlds) {
            continue;
        }

        let src = entry.path();
        let dst = to.join(entry.file_name());
        // Symlinks are not followed: one pointing outside the source would turn
        // the copy into a way of reading the node's filesystem.
        let meta = std::fs::symlink_metadata(&src)?;
        if meta.file_type().is_symlink() {
            continue;
        }

        if meta.is_dir() {
            std::fs::create_dir_all(&dst)?;
            copy_dir(&src, &dst, include_worlds, false, report)?;
        } else {
            std::fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
            report.files += 1;
            report.bytes += meta.len();
        }
    }
    Ok(())
}

fn skip(name: &str, include_worlds: bool) -> bool {
    if NEVER.contains(&name) {
        return true;
    }
    !include_worlds && name.starts_with(WORLD_PREFIX)
}

#[cfg(test)]
#[path = "clone_tests.rs"]
mod tests;
