use std::path::PathBuf;

use tokio::sync::{mpsc, oneshot};

/// A typed command line and the channel its one-line reply is sent on.
pub type Request = (String, oneshot::Sender<String>);

/// A Unix socket on which processes of the same user run typed commands, one per line.
pub struct CommandSocket {
    path: PathBuf,
    requests: mpsc::UnboundedReceiver<Request>,
}

impl CommandSocket {
    /// `$HELIX_COMMAND_SOCKET`, or a socket identified by the canonical workspace root.
    #[cfg(unix)]
    fn path() -> Option<PathBuf> {
        use sha2::{Digest, Sha256};

        if let Some(path) = std::env::var_os("HELIX_COMMAND_SOCKET") {
            return Some(path.into());
        }
        // Tests must not touch the socket of a running editor.
        if cfg!(feature = "integration") {
            return None;
        }
        let root = helix_loader::find_workspace().0;
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let hash = Sha256::digest(root.to_str()?.as_bytes());
        let id: String = hash[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) => PathBuf::from(dir).join("helix"),
            // SAFETY: getuid has no preconditions and cannot fail.
            None => PathBuf::from(format!("/tmp/helix-{}", unsafe { libc::getuid() })),
        };
        Some(dir.join(format!("{id}.sock")))
    }

    /// Listens on the command socket unless another live editor owns it.
    #[cfg(unix)]
    pub fn bind() -> Option<Self> {
        use std::os::unix::fs::DirBuilderExt;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let path = Self::path()?;
        if path.exists() {
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                log::info!("another editor listens on {}", path.display());
                return None;
            }
            let _ = std::fs::remove_file(&path);
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir);
        }
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(err) => {
                log::warn!("failed to listen on {}: {err}", path.display());
                return None;
            }
        };

        let (tx, requests) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let stream = match listener.accept().await {
                    Ok((stream, _)) => stream,
                    Err(err) => {
                        log::warn!("command socket: {err}");
                        continue;
                    }
                };
                let tx: mpsc::UnboundedSender<Request> = tx.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = BufReader::new(read).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let (reply_tx, reply_rx) = oneshot::channel();
                        if tx.send((line, reply_tx)).is_err() {
                            break;
                        }
                        let Ok(reply) = reply_rx.await else {
                            break;
                        };
                        if write
                            .write_all(format!("{reply}\n").as_bytes())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
        });
        Some(Self { path, requests })
    }

    #[cfg(not(unix))]
    pub fn bind() -> Option<Self> {
        None
    }

    /// The next request, or never if there is no socket.
    pub async fn next(socket: &mut Option<Self>) -> Option<Request> {
        match socket {
            Some(socket) => socket.requests.recv().await,
            None => std::future::pending().await,
        }
    }
}

impl Drop for CommandSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
