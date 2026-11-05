//! The daemon's local socket: a Unix domain socket (mode 0600, in a 0700 folder) on Unix, a
//! named pipe that refuses remote clients on Windows.

use std::io;

use tokio::io::{AsyncRead, AsyncWrite};

pub trait Conn: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Conn for T {}

#[cfg(unix)]
mod imp {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use tokio::net::{UnixListener, UnixStream};

    use super::*;

    pub struct Listener(UnixListener);

    /// Bind `address` (a socket path). The caller holds the daemon lock, so a leftover socket
    /// file is stale and can be removed.
    pub fn bind(address: &str) -> io::Result<Listener> {
        let path = Path::new(address);
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Listener(listener))
    }

    impl Listener {
        pub async fn accept(&mut self) -> io::Result<impl Conn> {
            Ok(self.0.accept().await?.0)
        }
    }

    /// `NotFound` or `ConnectionRefused` when no daemon listens.
    pub async fn connect(address: &str) -> io::Result<impl Conn> {
        UnixStream::connect(address).await
    }

    pub fn cleanup(address: &str) {
        let _ = std::fs::remove_file(address);
    }
}

#[cfg(windows)]
mod imp {
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

    use super::*;

    /// `ERROR_PIPE_BUSY`: every instance is connected; a new one follows right away.
    const PIPE_BUSY: i32 = 231;

    pub struct Listener {
        address: String,
        next: NamedPipeServer,
    }

    pub fn bind(address: &str) -> io::Result<Listener> {
        let next = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(address)?;
        Ok(Listener {
            address: address.to_string(),
            next,
        })
    }

    impl Listener {
        pub async fn accept(&mut self) -> io::Result<impl Conn> {
            self.next.connect().await?;
            let fresh = ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.address)?;
            Ok(std::mem::replace(&mut self.next, fresh))
        }
    }

    pub async fn connect(address: &str) -> io::Result<impl Conn> {
        for _ in 0..50 {
            match ClientOptions::new().open(address) {
                Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                other => return other,
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the desk daemon's pipe stays busy",
        ))
    }

    pub fn cleanup(_address: &str) {}
}

pub use imp::{bind, cleanup, connect};

/// Whether a connect error just means "no daemon running".
pub fn not_running(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}
