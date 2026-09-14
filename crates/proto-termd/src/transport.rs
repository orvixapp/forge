//! Where clients connect: a Unix domain socket with owner-only permissions,
//! or a Windows named pipe. Everything above this module only sees an
//! `AsyncRead + AsyncWrite` stream.

use anyhow::{Context, Result};
use std::path::Path;

#[cfg(unix)]
pub struct Listener(tokio::net::UnixListener);

#[cfg(unix)]
impl Listener {
    /// Binds `path`, replacing a stale socket file but refusing to steal a
    /// live daemon's.
    pub async fn bind(path: &Path) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            if tokio::net::UnixStream::connect(path).await.is_ok() {
                anyhow::bail!(
                    "another terminal daemon is already listening at {}",
                    path.display()
                );
            }
            std::fs::remove_file(path).context("remove stale socket")?;
        }
        let listener = tokio::net::UnixListener::bind(path)
            .with_context(|| format!("bind {}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .context("restrict socket permissions")?;
        Ok(Self(listener))
    }

    pub async fn accept(&mut self) -> Result<tokio::net::UnixStream> {
        let (stream, _) = self.0.accept().await?;
        Ok(stream)
    }
}

/// Named pipe server. `path` is the pipe name (`\\.\pipe\forge-termd-…`);
/// a new instance is created after each accepted client, which is how
/// Windows multiplexes connections on one name.
#[cfg(windows)]
pub struct Listener {
    name: String,
    next: tokio::net::windows::named_pipe::NamedPipeServer,
}

#[cfg(windows)]
impl Listener {
    /// Same signature as the Unix listener; creating a pipe is synchronous.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn bind(path: &Path) -> Result<Self> {
        use tokio::net::windows::named_pipe::ServerOptions;
        let name = pipe_name(path);
        let next = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .with_context(|| format!("create named pipe {name} (another daemon running?)"))?;
        Ok(Self { name, next })
    }

    pub async fn accept(&mut self) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
        use tokio::net::windows::named_pipe::ServerOptions;
        self.next.connect().await.context("wait for pipe client")?;
        let replacement = ServerOptions::new()
            .create(&self.name)
            .context("create next pipe instance")?;
        Ok(std::mem::replace(&mut self.next, replacement))
    }
}

/// A bare path becomes a pipe name; a full `\\.\pipe\…` is used as is.
#[cfg(windows)]
pub fn pipe_name(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.starts_with(r"\\.\pipe\") {
        return text.into_owned();
    }
    let stem = path.file_name().map_or_else(
        || "forge-termd".into(),
        |name| name.to_string_lossy().replace(['\\', '/', ':'], "-"),
    );
    format!(r"\\.\pipe\{stem}")
}
