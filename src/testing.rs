//! Helpers for unit tests.

use std::path::{Path, PathBuf};

/// A directory under the system's temporary directory, removed when
/// dropped. Tests never touch the real home or XDG directories.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("to11y-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Writes `text` to `relative`, creating its parent directories.
    pub fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    /// Writes an executable shell script to `relative`.
    pub fn script(&self, relative: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.write(relative, &format!("#!/bin/sh\n{body}\n"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A test-only certificate for `localhost` and `127.0.0.1`, self-signed,
/// and the key it was made with.
const CERT: &[u8] = include_bytes!("../tests/tls/localhost.cert.der");
const KEY: &[u8] = include_bytes!("../tests/tls/localhost.key.der");

/// A client that trusts only the test certificate.
pub fn https_client() -> crate::http::Client {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(CERT))
        .unwrap();
    crate::http::Client::with_roots(roots, std::time::Duration::from_secs(5))
}

/// An HTTPS server on `127.0.0.1` that answers one connection with
/// `response`, as raw bytes. It returns the port and, once the connection
/// ends, the request it read.
pub fn https_server(response: Vec<u8>) -> (u16, std::thread::JoinHandle<Vec<u8>>) {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::io::{Read, Write};
    use std::sync::Arc;

    let provider = Arc::new(rustls_graviola::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(CERT)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY)),
        )
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
        let mut stream = rustls::StreamOwned::new(connection, tcp);
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        // The head, then as much body as it declares.
        loop {
            let n = stream.read(&mut buffer).unwrap();
            request.extend_from_slice(&buffer[..n]);
            let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                assert!(n > 0, "the connection closed mid-head");
                continue;
            };
            let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |value| value.trim().parse().unwrap());
            if request.len() >= end + 4 + length || n == 0 {
                break;
            }
        }
        stream.write_all(&response).unwrap();
        stream.flush().unwrap();
        stream.conn.send_close_notify();
        let _ = stream.flush();
        request
    });
    (port, handle)
}
