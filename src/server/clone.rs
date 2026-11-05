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
    // The source directory is opened and checked once, then listed through
    // that handle: the source server may be running and could swap a
    // directory for a link to a host one mid-copy, which would carry host
    // files into a server its owner can download from.
    let dir = crate::fs::nofollow::Dir::open(from)
        .with_context(|| format!("opening {}", from.display()))?;
    for entry in dir
        .read_dir()
        .with_context(|| format!("reading {}", from.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        if at_root && skip(&name.to_string_lossy(), include_worlds) {
            continue;
        }

        let src = from.join(&name);
        let dst = to.join(&name);
        // Symlinks are not followed: one pointing outside the source would turn
        // the copy into a way of reading the node's filesystem.
        let meta = entry.metadata()?;
        if meta.file_type().is_symlink() {
            continue;
        }

        if meta.is_dir() {
            // A link in the target where a directory goes would carry the copy
            // out of the target server; the clone owns that spot, so the link
            // goes.
            if std::fs::symlink_metadata(&dst).is_ok_and(|m| m.file_type().is_symlink()) {
                std::fs::remove_file(&dst)?;
            }
            std::fs::create_dir_all(&dst)?;
            copy_dir(&src, &dst, include_worlds, false, report)?;
        } else if meta.is_file() {
            let mut input = crate::fs::nofollow::open_existing(&src, false)
                .with_context(|| format!("opening {}", src.display()))?;
            let mut output = crate::fs::nofollow::create_replacing(&dst)
                .with_context(|| format!("creating {}", dst.display()))?;
            report.bytes += std::io::copy(&mut input, &mut output)
                .with_context(|| format!("copying {}", src.display()))?;
            report.files += 1;
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
