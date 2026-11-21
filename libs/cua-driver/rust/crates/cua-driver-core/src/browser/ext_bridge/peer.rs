// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Who may connect to the bridge socket (spec §5 step 4, ADR D3).
//!
//! A connecting process is accepted only when
//! - it runs as the driver's own uid,
//! - its executable is the same file as the driver's (host and driver are
//!   one binary, so this pins the host without a configured path),
//! - its parent runs one of the configured browser executables, and
//! - that parent is the browser process itself, not a renderer or utility
//!   child (no `--type=` argument).
//!
//! Files are compared by device and inode, not by path. The process table
//! root is a parameter: production passes `/proc`; tests pass a directory
//! laid out the same way.
//!
//! pid reuse: the peer's start time is read before and after the checks; a
//! change means the pid now names a different process and the peer is
//! refused.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Identity of a file: device and inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

impl FileId {
    /// Identity of the file `path` resolves to (symlinks followed).
    pub fn of(path: &Path) -> std::io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
}

/// Why a peer was refused. Each value is stable and asserted by tests.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PeerRejection {
    #[error("peer uid {actual} is not the driver uid {expected}")]
    UidMismatch { expected: u32, actual: u32 },
    #[error("peer pid is unavailable")]
    PidUnavailable,
    #[error("peer process {pid} cannot be inspected")]
    ProcessGone { pid: i32 },
    #[error("peer process {pid} changed while it was being checked")]
    ProcessChanged { pid: i32 },
    #[error("peer executable is not the pinned host binary")]
    HostExeMismatch,
    #[error("peer parent is not a configured browser executable")]
    ParentNotBrowser,
    #[error("peer parent is a browser child process (--type=)")]
    ParentIsChildProcess,
}

/// What a peer must match.
#[derive(Debug, Clone)]
pub struct PeerPolicy {
    pub proc_root: PathBuf,
    pub uid: u32,
    pub host_exe: FileId,
    pub browsers: Vec<FileId>,
}

impl PeerPolicy {
    /// The production policy: real `/proc`, this process's uid and
    /// executable, and the configured browser paths that exist.
    pub fn for_this_process(browser_executables: &[PathBuf]) -> std::io::Result<Self> {
        Ok(Self {
            proc_root: PathBuf::from("/proc"),
            uid: unsafe { libc::geteuid() },
            host_exe: FileId::of(Path::new("/proc/self/exe"))?,
            browsers: browser_executables
                .iter()
                .filter_map(|path| FileId::of(path).ok())
                .collect(),
        })
    }
}

struct Stat {
    ppid: i32,
    start_time: u64,
}

fn read_stat(proc_root: &Path, pid: i32) -> Option<Stat> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    // `pid (comm) state ppid ...`: comm may contain spaces and parentheses,
    // so fields are counted after the last ')'.
    let rest = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // fields[0] is field 3 (state); ppid is field 4, starttime field 22.
    Some(Stat {
        ppid: fields.get(1)?.parse().ok()?,
        start_time: fields.get(19)?.parse().ok()?,
    })
}

/// Checks the process `pid` (credentials from `SO_PEERCRED`) against
/// `policy`.
pub fn verify_peer(policy: &PeerPolicy, uid: u32, pid: Option<i32>) -> Result<(), PeerRejection> {
    if uid != policy.uid {
        return Err(PeerRejection::UidMismatch {
            expected: policy.uid,
            actual: uid,
        });
    }
    let pid = pid
        .filter(|pid| *pid > 0)
        .ok_or(PeerRejection::PidUnavailable)?;
    let root = &policy.proc_root;
    let before = read_stat(root, pid).ok_or(PeerRejection::ProcessGone { pid })?;

    let exe = FileId::of(&root.join(pid.to_string()).join("exe"))
        .map_err(|_| PeerRejection::ProcessGone { pid })?;
    if exe != policy.host_exe {
        return Err(PeerRejection::HostExeMismatch);
    }

    let parent = root.join(before.ppid.to_string());
    let parent_exe =
        FileId::of(&parent.join("exe")).map_err(|_| PeerRejection::ParentNotBrowser)?;
    if !policy.browsers.contains(&parent_exe) {
        return Err(PeerRejection::ParentNotBrowser);
    }
    let cmdline =
        std::fs::read(parent.join("cmdline")).map_err(|_| PeerRejection::ParentNotBrowser)?;
    if cmdline
        .split(|byte| *byte == 0)
        .any(|arg| arg.starts_with(b"--type="))
    {
        return Err(PeerRejection::ParentIsChildProcess);
    }

    let after = read_stat(root, pid).ok_or(PeerRejection::ProcessGone { pid })?;
    if after.start_time != before.start_time || after.ppid != before.ppid {
        return Err(PeerRejection::ProcessChanged { pid });
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fake process table: `<root>/<pid>/{stat,exe}` for the host and
    /// `<root>/<ppid>/{exe,cmdline}` for its parent, with `exe` symlinks to
    /// two distinct files.
    pub(crate) struct FakeProc {
        pub dir: tempfile::TempDir,
        pub host_bin: PathBuf,
        pub browser_bin: PathBuf,
        pub other_bin: PathBuf,
    }

    pub(crate) const PARENT_PID: i32 = 7_000_001;

    impl FakeProc {
        pub(crate) fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let bins = dir.path().join("bin");
            std::fs::create_dir(&bins).unwrap();
            let make = |name: &str| {
                let path = bins.join(name);
                std::fs::write(&path, name).unwrap();
                path
            };
            Self {
                host_bin: make("host"),
                browser_bin: make("browser"),
                other_bin: make("other"),
                dir,
            }
        }

        pub(crate) fn root(&self) -> PathBuf {
            self.dir.path().join("proc")
        }

        /// Lays out `pid` as a child of [`PARENT_PID`].
        pub(crate) fn process(
            &self,
            pid: i32,
            exe: &Path,
            start_time: u64,
            parent_exe: &Path,
            parent_args: &[&str],
        ) {
            let me = self.root().join(pid.to_string());
            let _ = std::fs::remove_dir_all(&me);
            std::fs::create_dir_all(&me).unwrap();
            let stat = format!(
                "{pid} (cua driver) (x) S {PARENT_PID} 1 1 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 {start_time} 1000 10 18446744073709551615\n"
            );
            std::fs::write(me.join("stat"), stat).unwrap();
            std::os::unix::fs::symlink(exe, me.join("exe")).unwrap();

            let parent = self.root().join(PARENT_PID.to_string());
            let _ = std::fs::remove_dir_all(&parent);
            std::fs::create_dir_all(&parent).unwrap();
            std::os::unix::fs::symlink(parent_exe, parent.join("exe")).unwrap();
            let mut cmdline = Vec::new();
            for arg in parent_args {
                cmdline.extend_from_slice(arg.as_bytes());
                cmdline.push(0);
            }
            std::fs::write(parent.join("cmdline"), cmdline).unwrap();
        }

        pub(crate) fn policy(&self, uid: u32) -> PeerPolicy {
            PeerPolicy {
                proc_root: self.root(),
                uid,
                host_exe: FileId::of(&self.host_bin).unwrap(),
                browsers: vec![FileId::of(&self.browser_bin).unwrap()],
            }
        }
    }

    const PID: i32 = 4242;
    const UID: u32 = 1000;

    #[test]
    fn accepts_the_pinned_host_under_the_browser_process() {
        let proc = FakeProc::new();
        proc.process(
            PID,
            &proc.host_bin,
            99,
            &proc.browser_bin,
            &["browser", "--no-first-run"],
        );
        assert_eq!(verify_peer(&proc.policy(UID), UID, Some(PID)), Ok(()));
    }

    #[test]
    fn browser_path_matches_through_a_symlink() {
        let proc = FakeProc::new();
        let link = proc.dir.path().join("browser-link");
        std::os::unix::fs::symlink(&proc.browser_bin, &link).unwrap();
        proc.process(PID, &proc.host_bin, 99, &link, &["browser"]);
        assert_eq!(verify_peer(&proc.policy(UID), UID, Some(PID)), Ok(()));
    }

    #[test]
    fn each_rejection_has_its_own_value() {
        let proc = FakeProc::new();
        let policy = proc.policy(UID);

        proc.process(PID, &proc.other_bin, 99, &proc.browser_bin, &["browser"]);
        assert_eq!(
            verify_peer(&policy, UID, Some(PID)),
            Err(PeerRejection::HostExeMismatch)
        );

        proc.process(PID, &proc.host_bin, 99, &proc.other_bin, &["other"]);
        assert_eq!(
            verify_peer(&policy, UID, Some(PID)),
            Err(PeerRejection::ParentNotBrowser)
        );

        proc.process(
            PID,
            &proc.host_bin,
            99,
            &proc.browser_bin,
            &["browser", "--type=renderer"],
        );
        assert_eq!(
            verify_peer(&policy, UID, Some(PID)),
            Err(PeerRejection::ParentIsChildProcess)
        );

        proc.process(PID, &proc.host_bin, 99, &proc.browser_bin, &["browser"]);
        assert_eq!(
            verify_peer(&policy, UID + 1, Some(PID)),
            Err(PeerRejection::UidMismatch {
                expected: UID,
                actual: UID + 1
            })
        );
        assert_eq!(
            verify_peer(&policy, UID, None),
            Err(PeerRejection::PidUnavailable)
        );
        assert_eq!(
            verify_peer(&policy, UID, Some(PID + 1)),
            Err(PeerRejection::ProcessGone { pid: PID + 1 })
        );
    }

    #[test]
    fn type_flag_must_be_a_whole_argument_prefix() {
        let proc = FakeProc::new();
        proc.process(
            PID,
            &proc.host_bin,
            99,
            &proc.browser_bin,
            &["browser", "--flag=--type=renderer"],
        );
        assert_eq!(verify_peer(&proc.policy(UID), UID, Some(PID)), Ok(()));
    }
}
