//! Linux tray singleton and a deliberately small, per-user start-only control socket.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use fs2::FileExt;

const START: &[u8; 6] = b"START\n";
const ACCEPTED: &[u8; 3] = b"OK\n";
const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Owns the tray host, independently of whether it currently owns acquisition.
#[derive(Debug)]
pub(crate) struct TrayHost {
    lock: File,
    socket: PathBuf,
    listener: Option<UnixListener>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TrayHost {
    /// Return the new singleton host, or request Start from an existing host and
    /// return `None`. The existing host keeps its original service configuration.
    /// Custom acquisition lock paths get their own independent tray namespace.
    pub(crate) fn acquire_or_start(service_lock: &Path) -> Result<Option<Self>> {
        let mut directory_name = service_lock
            .file_name()
            .context("service lock has no filename")?
            .to_owned();
        directory_name.push(".tray");
        let directory = service_lock.with_file_name(directory_name);
        if let Ok(metadata) = fs::symlink_metadata(&directory) {
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "tray control path is not a real directory"
            );
        }
        fs::create_dir_all(&directory).context("create private tray control directory")?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(directory.join("host.lock"))
            .context("open tray host lock")?;
        lock.set_permissions(fs::Permissions::from_mode(0o600))?;
        let socket = directory.join("control.sock");
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            match FileExt::try_lock_exclusive(&lock) {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    match request_start(&socket) {
                        Ok(()) => return Ok(None),
                        Err(error) if retryable(&error) && Instant::now() < deadline => {
                            thread::sleep(Duration::from_millis(40));
                        }
                        Err(error) => {
                            return Err(error)
                                .context("could not ask the existing measurement tray to start");
                        }
                    }
                }
                Err(error) => return Err(error).context("acquire tray host lock"),
            }
        }
        // A crash may leave the socket pathname. Only the new lock owner may
        // remove it; contenders never delete another tray's active endpoint.
        match fs::remove_file(&socket) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove stale tray control socket"),
        }
        let listener = UnixListener::bind(&socket).context("bind private tray control socket")?;
        listener.set_nonblocking(true)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        Ok(Some(Self {
            lock,
            socket,
            listener: Some(listener),
            stopping: Arc::new(AtomicBool::new(false)),
            thread: None,
        }))
    }

    /// Begin forwarding requests to the native event loop after its proxy exists.
    pub(crate) fn listen(&mut self, start: impl Fn() -> bool + Send + 'static) -> Result<()> {
        let listener = self
            .listener
            .take()
            .context("tray control listener already started")?;
        let stopping = Arc::clone(&self.stopping);
        self.thread = Some(
            thread::Builder::new()
                .name("kasina-tray-control".to_owned())
                .spawn(move || {
                    for stream in listener.incoming() {
                        if stopping.load(Ordering::Acquire) {
                            break;
                        }
                        match stream {
                            Ok(mut stream) => {
                                if let Err(error) = handle_request(&mut stream, &start) {
                                    tracing::debug!(%error, "ignored invalid tray control request");
                                }
                            }
                            Err(error) => {
                                if error.kind() == io::ErrorKind::WouldBlock {
                                    thread::sleep(Duration::from_millis(20));
                                } else if error.kind() != io::ErrorKind::Interrupted {
                                    tracing::warn!(%error, "tray control listener failed");
                                    break;
                                }
                            }
                        }
                    }
                })
                .context("start tray control listener")?,
        );
        Ok(())
    }
}

impl Drop for TrayHost {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        // The nonblocking listener checks this flag frequently. Any incomplete
        // incoming command has a short read timeout as well.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.listener.take();
        let _ = fs::remove_file(&self.socket);
        let _ = FileExt::unlock(&self.lock);
    }
}

fn retryable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
            | io::ErrorKind::Interrupted
    )
}

fn request_start(socket: &Path) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    stream.write_all(START)?;
    let mut response = [0; 3];
    stream.read_exact(&mut response)?;
    if &response == ACCEPTED {
        Ok(())
    } else {
        Err(io::Error::other(
            "the existing tray is shutting down or rejected the request",
        ))
    }
}

fn handle_request(stream: &mut UnixStream, start: &impl Fn() -> bool) -> io::Result<()> {
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    let mut command = [0; 6];
    stream.read_exact(&mut command)?;
    if &command == START && start() {
        stream.write_all(ACCEPTED)
    } else {
        stream.write_all(b"NO\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn subsequent_launches_signal_one_host_and_release_all_resources_on_exit() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("service.lock");
        let mut host = TrayHost::acquire_or_start(&path).unwrap().unwrap();
        let socket = host.socket.clone();
        let (sender, received) = mpsc::channel();
        host.listen(move || sender.send(()).is_ok()).unwrap();
        for _ in 0..3 {
            assert!(TrayHost::acquire_or_start(&path).unwrap().is_none());
            received.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        assert_eq!(
            fs::metadata(socket.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            host.lock.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Acquisition ownership is intentionally independent of the tray host.
        drop(kasina_service::ServiceLock::acquire(&path).unwrap());
        drop(host);
        assert!(!socket.exists());
        assert!(TrayHost::acquire_or_start(&path).unwrap().is_some());
    }

    #[test]
    fn second_launch_waits_for_event_loop_startup_and_never_creates_another_host() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("service.lock");
        let mut host = TrayHost::acquire_or_start(&path).unwrap().unwrap();
        let contender = thread::spawn(move || TrayHost::acquire_or_start(&path).unwrap().is_none());
        thread::sleep(Duration::from_millis(100));
        let (sender, received) = mpsc::channel();
        host.listen(move || sender.send(()).is_ok()).unwrap();
        assert!(contender.join().unwrap());
        received.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn stale_sockets_are_recovered_only_by_the_lock_owner() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("service.lock");
        let host = TrayHost::acquire_or_start(&path).unwrap().unwrap();
        let socket = host.socket.clone();
        drop(host);
        drop(UnixListener::bind(&socket).unwrap());
        let replacement = TrayHost::acquire_or_start(&path).unwrap().unwrap();
        assert!(replacement.socket.exists());
    }

    #[test]
    fn control_protocol_only_accepts_start_and_does_not_invoke_stop() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("service.lock");
        let mut host = TrayHost::acquire_or_start(&path).unwrap().unwrap();
        let (sender, received) = mpsc::channel();
        host.listen(move || sender.send(()).is_ok()).unwrap();
        let mut stream = UnixStream::connect(&host.socket).unwrap();
        stream.write_all(b"STOP!\n").unwrap();
        let mut response = [0; 3];
        stream.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"NO\n");
        assert!(received.try_recv().is_err());
        request_start(&host.socket).unwrap();
        received.recv_timeout(Duration::from_secs(1)).unwrap();
    }
}
