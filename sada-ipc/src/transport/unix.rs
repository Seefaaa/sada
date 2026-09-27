//! Unix socket transport.

use std::{
    fs,
    io,
    os::unix::{
        fs::{FileTypeExt as _, MetadataExt as _},
        net,
    },
    path::{Path, PathBuf},
};

use tokio::net::{UnixListener, UnixStream};

use crate::Endpoint;

/// A bound unix socket, accepting connections.
#[derive(Debug)]
pub struct Listener {
    /// The bound socket.
    inner: UnixListener,
    /// Path of the socket file, kept so [`Drop`] can remove it.
    path: PathBuf,
    /// Device and inode of the socket file that was bound, if it could be read.
    ///
    /// What is at the path can be something else by the time this listener is dropped, and removing another process's
    /// socket would take its bridge down with it.
    file: Option<(u64, u64)>,
}

impl Listener {
    /// Bind the socket, replacing one a previous process left behind.
    ///
    /// A path something is still answering on is left alone and the bind fails with `AddrInUse`, and so is a path
    /// holding anything that is not a socket.
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        clear(endpoint.path())?;

        let inner = UnixListener::bind(endpoint.path())?;

        Ok(Self {
            inner,
            path: endpoint.path().to_owned(),
            file: identify(endpoint.path()),
        })
    }

    /// Wait for the next client.
    pub async fn accept(&mut self) -> io::Result<UnixStream> {
        let (stream, _) = self.inner.accept().await?;
        Ok(stream)
    }
}

impl Drop for Listener {
    /// Remove the socket file, leaving nothing for the next process to clear.
    ///
    /// Only the file this listener bound: what is at the path now may belong to whatever bound it next.
    fn drop(&mut self) {
        let current = identify(&self.path);

        if current.is_some() && current == self.file {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Device and inode of whatever is at `path`, which together name one file.
fn identify(path: &Path) -> Option<(u64, u64)> {
    let metadata = fs::metadata(path).ok()?;

    Some((metadata.dev(), metadata.ino()))
}

/// Make room at `path` for a socket to be bound.
///
/// What is removed is a socket nothing answers on, which is what a process that died without cleaning up leaves
/// behind. A socket with a server still on it, and anything that is not a socket at all, is an error instead.
fn clear(path: &Path) -> io::Result<()> {
    let file_type = match fs::metadata(path) {
        Ok(metadata) => metadata.file_type(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    if !file_type.is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} is not a socket", path.display()),
        ));
    }

    if net::UnixStream::connect(path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("{} is already being served", path.display()),
        ));
    }

    match fs::remove_file(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Connect to a bound socket.
pub async fn connect(endpoint: &Endpoint) -> io::Result<UnixStream> { UnixStream::connect(endpoint.path()).await }
