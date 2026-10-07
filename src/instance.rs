//! One process per user, independent of the executable's location (including
//! AppImage mounts and replaced bundles). Acquire before loading writable state.

use std::{
    fs::{self, DirBuilder, File, OpenOptions, TryLockError},
    io::{self, Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};

const SHOW: u8 = 1;
const TIMEOUT: Duration = Duration::from_secs(3);

pub struct Instance {
    // Never unlink the lock file: another process may already have opened it.
    // Dropping the descriptor (or exiting/crashing) releases the OS lock.
    _lock: File,
    socket: PathBuf,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    pub reopen: async_channel::Receiver<()>,
}

impl Instance {
    /// `None` means another process accepted this launch. Quiet login launches
    /// don't raise its window; ordinary launches queue a reopen request.
    pub fn acquire(background: bool) -> Result<Option<Self>> {
        // A short, stable socket path also works when HOME is too long for a
        // Unix socket. The private directory prevents other users sending IPC.
        // SAFETY: geteuid takes no arguments and has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let directory = PathBuf::from(format!("/tmp/octowatcher-{uid}"));
        match DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.into()),
        }
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            bail!(
                "{} must be a private directory owned by you",
                directory.display()
            );
        }
        Self::acquire_in(&directory, background)
    }

    fn acquire_in(directory: &Path, background: bool) -> Result<Option<Self>> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join("instance.lock"))?;
        let socket = directory.join("instance.sock");
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match lock.try_lock() {
                Ok(()) => return Self::own(lock, socket).map(Some),
                Err(TryLockError::Error(err)) => return Err(err.into()),
                Err(TryLockError::WouldBlock) => {}
            }
            if background {
                // No need to connect or activate an already-running owner.
                return Ok(None);
            }
            // The first process may still be binding its socket. If it dies,
            // try_lock above lets this launch take over instead.
            match send_reopen(&socket) {
                Ok(()) => return Ok(None),
                Err(err) if Instant::now() >= deadline => {
                    return Err(err)
                        .context("Octowatcher is running but did not accept a reopen request");
                }
                Err(_) => thread::sleep(Duration::from_millis(25)),
            }
        }
    }

    fn own(lock: File, socket: PathBuf) -> Result<Self> {
        // Only the lock owner removes a stale socket left after a crash.
        match fs::remove_file(&socket) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        let listener = UnixListener::bind(&socket).context("could not bind instance socket")?;
        let (sender, reopen) = async_channel::unbounded();
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = stopped.clone();
        let worker = thread::Builder::new()
            .name("octowatcher-reopen".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if worker_stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(mut stream) = stream else { break };
                    // A broken client mustn't monopolize the instance listener.
                    stream
                        .set_read_timeout(Some(Duration::from_millis(200)))
                        .ok();
                    stream
                        .set_write_timeout(Some(Duration::from_millis(200)))
                        .ok();
                    let mut message = [0];
                    if stream.read_exact(&mut message).is_ok()
                        && message[0] == SHOW
                        && sender.send_blocking(()).is_ok()
                    {
                        stream.write_all(&[SHOW]).ok();
                    }
                }
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(err) => {
                fs::remove_file(&socket).ok();
                return Err(err.into());
            }
        };
        Ok(Self {
            _lock: lock,
            socket,
            stopped,
            worker: Some(worker),
            reopen,
        })
    }
}

fn send_reopen(socket: &Path) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    stream.set_write_timeout(Some(Duration::from_millis(250)))?;
    stream.write_all(&[SHOW])?;
    let mut ack = [0];
    stream.read_exact(&mut ack)?;
    if ack[0] != SHOW {
        return Err(io::Error::other("invalid instance acknowledgement"));
    }
    Ok(())
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        // Wake accept before joining; the lock stays held through cleanup.
        UnixStream::connect(&self.socket).ok();
        if let Some(worker) = self.worker.take() {
            worker.join().ok();
        }
        fs::remove_file(&self.socket).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn quiet_duplicates_dont_reopen_and_ordinary_duplicates_do() {
        let directory = tempfile::tempdir().unwrap();
        let owner = Instance::acquire_in(directory.path(), true)
            .unwrap()
            .unwrap();
        assert!(
            Instance::acquire_in(directory.path(), true)
                .unwrap()
                .is_none()
        );
        assert!(owner.reopen.try_recv().is_err());
        assert!(
            Instance::acquire_in(directory.path(), false)
                .unwrap()
                .is_none()
        );
        assert_eq!(owner.reopen.try_recv(), Ok(()));
        drop(owner);
        // Quit/restart keeps the lock inode but releases ownership.
        assert!(directory.path().join("instance.lock").exists());
        assert!(
            Instance::acquire_in(directory.path(), false)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn concurrent_launches_choose_exactly_one_owner() {
        let directory = tempfile::tempdir().unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let launches: Vec<_> = (0..8)
            .map(|_| {
                let directory = directory.path().to_path_buf();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    Instance::acquire_in(&directory, true).unwrap()
                })
            })
            .collect();
        // Retain results so the owner can't drop its lock before another
        // contender has finished.
        let owners: Vec<_> = launches
            .into_iter()
            .map(|launch| launch.join().unwrap())
            .collect();
        assert_eq!(owners.iter().filter(|owner| owner.is_some()).count(), 1);
    }

    // Runs as a separate process only when the parent test supplies a private
    // directory. It never creates a GPUI app, tray, state file, or GitHub request.
    #[test]
    fn child_instance() {
        let Some(directory) = std::env::var_os("OCTOWATCHER_TEST_INSTANCE") else {
            return;
        };
        let directory = PathBuf::from(directory);
        let owner = Instance::acquire_in(&directory, true).unwrap().unwrap();
        fs::write(directory.join("ready"), "ready").unwrap();
        owner.reopen.recv_blocking().unwrap();
        fs::write(directory.join("opened"), "opened").unwrap();
        loop {
            thread::park();
        }
    }

    #[test]
    fn process_lock_handoff_and_crash_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "instance::tests::child_instance"])
            .env("OCTOWATCHER_TEST_INSTANCE", directory.path())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let result = std::panic::catch_unwind(|| {
            wait_for(&directory.path().join("ready"));
            assert!(
                Instance::acquire_in(directory.path(), true)
                    .unwrap()
                    .is_none()
            );
            assert!(!directory.path().join("opened").exists());
            assert!(
                Instance::acquire_in(directory.path(), false)
                    .unwrap()
                    .is_none()
            );
            wait_for(&directory.path().join("opened"));
        });
        child.kill().unwrap();
        child.wait().unwrap();
        result.unwrap();
        assert!(directory.path().join("instance.sock").exists());
        let restarted = Instance::acquire_in(directory.path(), false)
            .unwrap()
            .unwrap();
        assert!(
            Instance::acquire_in(directory.path(), false)
                .unwrap()
                .is_none()
        );
        assert_eq!(restarted.reopen.try_recv(), Ok(()));
    }

    fn wait_for(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
