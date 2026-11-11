use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, OnceLock, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};

use iced::futures::{SinkExt, Stream};
use tokio::sync::Notify;

static ACTIVATION: OnceLock<Arc<Notify>> = OnceLock::new();

fn activation() -> Arc<Notify> { ACTIVATION.get_or_init(|| Arc::new(Notify::new())).clone() }

pub fn activation_events() -> impl Stream<Item = crate::Message>
{
    iced::stream::channel(1, async move |mut output| {
        let activation = activation();
        loop {
            activation.notified().await;
            if output.send(crate::Message::ActivateWindow).await.is_err() { break; }
        }
    })
}

pub struct Instance
{
    // Never unlink this file: another opener must lock the same inode, including
    // after a crash. The kernel releases the lock when the process/handle ends.
    _lock: File,
    stopped: Arc<AtomicBool>,
    listener: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Instance
{
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(listener) = self.listener.take() {
            listener.thread().unpark();
            let _ = listener.join();
        }
    }
}

pub fn acquire() -> io::Result<Option<Instance>>
{
    let path = crate::storage::config_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Cannot locate Snack's configuration directory"))?
        .with_file_name("instance.lock");
    acquire_at(&path, activation())
}

fn acquire_at(path: &Path, activation: Arc<Notify>) -> io::Result<Option<Instance>>
{
    let parent = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Missing instance directory"))?;
    crate::storage::private_directory(parent)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if !metadata.is_file() { return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid instance lock file")); }
    }
    let lock = options.open(path)?;
    match lock.try_lock() {
        Ok(()) => {},
        Err(TryLockError::WouldBlock) => {
            match activate_existing(&path.with_extension("endpoint")) {
                Ok(()) => return Ok(None),
                Err(activation_error) => match lock.try_lock() {
                    // The previous instance may have exited during activation.
                    Ok(()) => {},
                    Err(TryLockError::WouldBlock) => return Err(activation_error),
                    Err(TryLockError::Error(error)) => return Err(error),
                },
            }
        }
        Err(TryLockError::Error(error)) => return Err(error),
    }

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let token = uuid::Uuid::new_v4().to_string();
    let endpoint = format!("{} {token}", listener.local_addr()?.port());
    // Separate metadata from the locked file: Windows exclusive file locks can
    // also deny reads. Atomic replacement avoids a partial startup endpoint.
    crate::storage::save_config_file(&path.with_extension("endpoint"), endpoint.as_bytes())?;
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = stopped.clone();
    let thread = std::thread::Builder::new().name("snack-activation".into()).spawn(move || {
        while !stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                    let mut request = [0; 36];
                    if stream.read_exact(&mut request).is_ok() && request == token.as_bytes() {
                        activation.notify_one();
                        let _ = stream.write_all(&[1]);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => std::thread::park_timeout(Duration::from_millis(20)),
                Err(error) => { log::warn!("Activation listener failed: {error}"); break; }
            }
        }
    })?;
    Ok(Some(Instance { _lock: lock, stopped, listener: Some(thread) }))
}

fn activate_existing(path: &Path) -> io::Result<()>
{
    // The first process may hold its lock before it has published its endpoint.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let attempt = || -> io::Result<()> {
            let mut endpoint = String::new();
            File::open(path)?.take(128).read_to_string(&mut endpoint)?;
            let (port, token) = endpoint.split_once(' ').ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid activation endpoint"))?;
            let port: u16 = port.parse().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid activation port"))?;
            if token.len() != 36 { return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid activation token")); }
            uuid::Uuid::parse_str(token).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid activation token"))?;
            let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(200))?;
            stream.set_write_timeout(Some(Duration::from_millis(200)))?;
            stream.set_read_timeout(Some(Duration::from_millis(200)))?;
            stream.write_all(token.as_bytes())?;
            let mut ack = [0];
            stream.read_exact(&mut ack)?;
            if ack != [1] { return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid activation acknowledgement")); }
            Ok(())
        };
        match attempt() {
            Ok(()) => return Ok(()),
            Err(error) if Instant::now() >= deadline => return Err(io::Error::new(error.kind(), format!("Snack is already running, but its window could not be activated: {error}"))),
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[cfg(test)]
mod tests
{
    use super::*;
    use iced::futures::FutureExt;
    use std::process::Command;

    // Invoked only by the parent regression, never through the application main
    // function: no UI, credentials, real user configuration, or XMPP connection.
    #[test]
    fn subprocess_probe() {
        let Some(path) = std::env::var_os("SNACK_TEST_INSTANCE_PATH") else { return; };
        let instance = acquire_at(Path::new(&path), Arc::new(Notify::new())).unwrap();
        match std::env::var("SNACK_TEST_INSTANCE_MODE").unwrap().as_str() {
            "activate" => assert!(instance.is_none()),
            "hold" => {
                assert!(instance.is_some());
                std::fs::write(Path::new(&path).with_extension("ready"), b"ready").unwrap();
                loop { std::thread::park(); }
            }
            mode => panic!("Unexpected probe mode: {mode}"),
        }
    }

    fn probe(path: &Path, mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "instance::tests::subprocess_probe", "--nocapture"])
            .env("SNACK_TEST_INSTANCE_PATH", path).env("SNACK_TEST_INSTANCE_MODE", mode);
        command
    }

    #[test]
    fn second_process_activates_the_owner_and_crash_releases_its_lock() {
        let directory = std::env::temp_dir().join(format!("snack-instance-{}", uuid::Uuid::new_v4()));
        let path = directory.join("instance.lock");
        let notify = Arc::new(Notify::new());
        let first = acquire_at(&path, notify.clone()).unwrap().unwrap();
        let original_config = directory.join("config.toml");
        std::fs::write(&original_config, b"untouched drafts").unwrap();
        let output = probe(&path, "activate").output().unwrap();
        assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(notify.notified().now_or_never().is_some());
        assert_eq!(std::fs::read(&original_config).unwrap(), b"untouched drafts");
        drop(first);

        struct Child(std::process::Child);
        impl Drop for Child { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
        let mut child = Child(probe(&path, "hold").stdout(std::process::Stdio::null()).spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.with_extension("ready").exists() {
            assert!(child.0.try_wait().unwrap().is_none(), "Lock holder exited early");
            assert!(Instant::now() < deadline, "Lock holder did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        // A stale file must not become a stale lock after abnormal termination.
        let recovered = acquire_at(&path, Arc::new(Notify::new())).unwrap().unwrap();
        drop(recovered);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn activation_requires_the_private_token_and_survives_an_incomplete_client() {
        let directory = std::env::temp_dir().join(format!("snack-activation-{}", uuid::Uuid::new_v4()));
        let path = directory.join("instance.lock");
        let notify = Arc::new(Notify::new());
        let first = acquire_at(&path, notify.clone()).unwrap().unwrap();
        let endpoint = std::fs::read_to_string(path.with_extension("endpoint")).unwrap();
        let port: u16 = endpoint.split_once(' ').unwrap().0.parse().unwrap();
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        client.write_all(uuid::Uuid::new_v4().to_string().as_bytes()).unwrap();
        let mut ack = [0];
        assert!(client.read_exact(&mut ack).is_err());
        assert!(notify.notified().now_or_never().is_none());
        let incomplete = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        // The read deadline lets the valid activation queued behind it proceed.
        assert!(acquire_at(&path, notify.clone()).unwrap().is_none());
        assert!(notify.notified().now_or_never().is_some());
        drop(incomplete);
        drop(first);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
