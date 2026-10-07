//! One process per user, independent of the executable's location (including
//! AppImage mounts and replaced bundles). Acquire before loading writable state.

use std::{
    fs::{self, DirBuilder, File, OpenOptions, TryLockError},
    io::{self, Read, Write},
    os::unix::{
        fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt},
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
    _socket_directory: tempfile::TempDir,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    pub reopen: async_channel::Receiver<()>,
}

impl Instance {
    /// `None` means another process accepted this launch. Quiet login launches
    /// don't raise its window; ordinary launches queue a reopen request.
    pub fn acquire(background: bool) -> Result<Option<Self>> {
        // Prefer the stable home location; use the OS's private per-user
        // directory when HOME permits other users to create/replace entries.
        // Neither location depends on XDG_CONFIG_HOME or the executable.
        let home = dirs::home_dir().context("no home directory for instance lock")?;
        let runtime = runtime_directory();
        let directory = instance_directory(&home, runtime.as_deref())?;
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
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match lock.try_lock() {
                Ok(()) => return Self::own(lock).map(Some),
                Err(TryLockError::Error(err)) => return Err(err.into()),
                Err(TryLockError::WouldBlock) => {}
            }
            if background {
                // No need to connect or activate an already-running owner.
                return Ok(None);
            }
            // The first process may still be binding its socket. If it dies,
            // try_lock above lets this launch take over instead.
            match read_socket(&lock).and_then(|socket| send_reopen(&socket)) {
                Ok(()) => return Ok(None),
                Err(err) if Instant::now() >= deadline => {
                    return Err(err)
                        .context("Octowatcher is running but did not accept a reopen request");
                }
                Err(_) => thread::sleep(Duration::from_millis(25)),
            }
        }
    }

    fn own(lock: File) -> Result<Self> {
        // The lock lives in HOME; the random private socket directory keeps
        // Unix socket paths short even with a very long home directory.
        if let Ok(stale) = read_socket(&lock) {
            cleanup_socket(&stale);
        }
        let socket_directory = tempfile::Builder::new()
            .prefix("octowatcher-ipc-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(fs::canonicalize("/tmp")?)?;
        let socket = socket_directory.path().join("instance.sock");
        let listener = UnixListener::bind(&socket).context("could not bind instance socket")?;
        lock.set_len(0)?;
        lock.write_all_at(socket.as_os_str().as_encoded_bytes(), 0)?;
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
            _socket_directory: socket_directory,
            stopped,
            worker: Some(worker),
            reopen,
        })
    }
}

fn current_uid() -> libc::uid_t {
    // SAFETY: geteuid has no arguments or preconditions.
    unsafe { libc::geteuid() }
}

#[cfg(target_os = "linux")]
fn runtime_directory() -> Option<PathBuf> {
    // Login and terminal launches normally share XDG_RUNTIME_DIR. The
    // conventional OS path also works when a shell omits that variable.
    dirs::runtime_dir().or_else(|| Some(PathBuf::from(format!("/run/user/{}", current_uid()))))
}

#[cfg(target_os = "macos")]
fn runtime_directory() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    // Darwin's private user cache is stable across launches and, unlike its
    // temporary directory, is not periodically cleaned while a lock is held.
    // SAFETY: a zero length query accepts a null buffer.
    let size = unsafe { libc::confstr(libc::_CS_DARWIN_USER_CACHE_DIR, std::ptr::null_mut(), 0) };
    if size == 0 {
        return None;
    }
    let mut bytes = vec![0; size];
    // SAFETY: bytes has the queried buffer size and remains live for this call.
    let written = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_CACHE_DIR,
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if written == 0 || written > bytes.len() {
        return None;
    }
    bytes.truncate(written - 1); // confstr's size includes the final NUL.
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

fn protected_parent(path: &Path, private: bool) -> bool {
    path.is_absolute()
        && fs::metadata(path).is_ok_and(|metadata| {
            metadata.is_dir()
                && metadata.uid() == current_uid()
                && metadata.mode() & if private { 0o077 } else { 0o022 } == 0
        })
}

fn instance_directory(home: &Path, runtime: Option<&Path>) -> Result<PathBuf> {
    if protected_parent(home, false) {
        return private_directory(home);
    }
    let runtime = runtime
        .filter(|path| protected_parent(path, true))
        .context("instance lock needs a protected home or a private per-user runtime directory")?;
    private_directory(runtime)
}

fn private_directory(home: &Path) -> Result<PathBuf> {
    // Validate the parent too: a private leaf in a shared writable directory
    // could already have been reserved by another local user.
    let uid = current_uid();
    if !protected_parent(home, false) {
        bail!("the instance lock directory must be owned by you and not writable by other users");
    }
    let directory = home.join(".octowatcher-instance");
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
    Ok(directory)
}

fn read_socket(lock: &File) -> io::Result<PathBuf> {
    let mut contents = [0; 256];
    let size = lock.read_at(&mut contents, 0)?;
    let path = std::str::from_utf8(&contents[..size])
        .map_err(|_| io::Error::other("invalid instance socket path"))?;
    if path.is_empty() {
        return Err(io::Error::other("instance socket is not ready"));
    }
    Ok(PathBuf::from(path))
}

fn cleanup_socket(socket: &Path) {
    let Some(directory) = socket.parent() else {
        return;
    };
    // Only remove the exact private socket directory we create, never an
    // arbitrary path from a stale lock file. remove_dir refuses nonempty dirs.
    if socket
        .file_name()
        .is_some_and(|name| name == "instance.sock")
        && directory.parent() == fs::canonicalize("/tmp").ok().as_deref()
        && directory
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("octowatcher-ipc-"))
        && fs::symlink_metadata(directory).is_ok_and(|metadata| {
            metadata.is_dir() && metadata.uid() == current_uid() && metadata.mode() & 0o077 == 0
        })
    {
        fs::remove_file(socket).ok();
        fs::remove_dir(directory).ok();
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
        let stale =
            read_socket(&File::open(directory.path().join("instance.lock")).unwrap()).unwrap();
        assert!(stale.exists());
        let restarted = Instance::acquire_in(directory.path(), false)
            .unwrap()
            .unwrap();
        assert!(!stale.exists());
        assert!(
            Instance::acquire_in(directory.path(), false)
                .unwrap()
                .is_none()
        );
        assert_eq!(restarted.reopen.try_recv(), Ok(()));
    }

    #[test]
    fn lock_is_private_and_long_home_does_not_lengthen_socket() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("long-home".repeat(20));
        fs::create_dir(&home).unwrap();
        let directory = private_directory(&home).unwrap();
        assert!(directory.starts_with(&home));
        assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
        let owner = Instance::acquire_in(&directory, false).unwrap().unwrap();
        assert!(owner.socket.as_os_str().len() < 100);
        assert_eq!(
            fs::metadata(owner.socket.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        assert!(Instance::acquire_in(&directory, false).unwrap().is_none());
        assert_eq!(owner.reopen.try_recv(), Ok(()));
    }

    #[test]
    fn refuses_shared_parent_and_symlinked_private_directory() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(private_directory(home.path()).is_err());
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let target = tempfile::tempdir().unwrap();
        symlink(target.path(), home.path().join(".octowatcher-instance")).unwrap();
        assert!(private_directory(home.path()).is_err());
    }

    #[test]
    fn group_writable_home_uses_private_runtime_and_hands_off_launches() {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o775)).unwrap();
        let runtime = tempfile::tempdir().unwrap();
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let directory = instance_directory(home.path(), Some(runtime.path())).unwrap();
        assert!(directory.starts_with(runtime.path()));
        assert!(!home.path().join(".octowatcher-instance").exists());
        let owner = Instance::acquire_in(&directory, true).unwrap().unwrap();
        assert!(Instance::acquire_in(&directory, true).unwrap().is_none());
        assert!(owner.reopen.try_recv().is_err());
        assert!(Instance::acquire_in(&directory, false).unwrap().is_none());
        assert_eq!(owner.reopen.try_recv(), Ok(()));
        drop(owner);
        assert!(Instance::acquire_in(&directory, false).unwrap().is_some());
        // A protected HOME keeps the same lock regardless of runtime env.
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            instance_directory(home.path(), Some(runtime.path()))
                .unwrap()
                .starts_with(home.path())
        );
    }

    #[test]
    fn unprotected_home_requires_a_private_runtime_parent() {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o775)).unwrap();
        assert!(instance_directory(home.path(), None).is_err());
        let runtime = tempfile::tempdir().unwrap();
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o775)).unwrap();
        assert!(instance_directory(home.path(), Some(runtime.path())).is_err());
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let target = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(target.path(), runtime.path().join(".octowatcher-instance"))
            .unwrap();
        assert!(instance_directory(home.path(), Some(runtime.path())).is_err());
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
