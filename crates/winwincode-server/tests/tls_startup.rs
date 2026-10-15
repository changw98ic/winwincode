// SPDX-License-Identifier: Apache-2.0

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

struct Fixture {
    directory: PathBuf,
    server: Child,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn initialize_repository(directory: &Path) {
    std::fs::create_dir_all(directory).unwrap();
    for arguments in [
        vec!["init", "--quiet"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    ] {
        assert!(
            Command::new("git")
                .args(arguments)
                .current_dir(directory)
                .status()
                .unwrap()
                .success()
        );
    }
}

fn https_post(
    address: SocketAddr,
    client: &Arc<rustls::ClientConfig>,
    path: &str,
    body: &Value,
    bootstrap: bool,
) -> Result<(u16, Value), Box<dyn std::error::Error>> {
    let socket = TcpStream::connect_timeout(&address, Duration::from_millis(200))?;
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let name = rustls::pki_types::ServerName::IpAddress(address.ip().into());
    let connection = rustls::ClientConnection::new(Arc::clone(client), name)?;
    let mut stream = rustls::StreamOwned::new(connection, socket);
    let body = serde_json::to_vec(body)?;
    let authorization = if bootstrap {
        "Authorization: Bearer bootstrap-proof\r\n"
    } else {
        ""
    };
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nOrigin: https://{address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{authorization}Connection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or("HTTP response head")?;
    let status = head
        .split_whitespace()
        .nth(1)
        .ok_or("HTTP response status")?
        .parse()?;
    Ok((status, serde_json::from_str(body)?))
}

#[test]
#[allow(clippy::too_many_lines)]
fn real_main_starts_bootstrap_and_product_https_with_both_crypto_providers() {
    let directory = std::env::temp_dir().join(format!(
        "wwc-main-https-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let repository = directory.join("repository");
    initialize_repository(&repository);
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let certificate = directory.join("certificate.pem");
    let key = directory.join("key.pem");
    std::fs::write(&certificate, cert.pem()).unwrap();
    std::fs::write(&key, signing_key.serialize_pem()).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der().clone()).unwrap();
    let client = Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let log = directory.join("server.log");
    let server = Command::new(env!("CARGO_BIN_EXE_winwincode-server"))
        .env("WWC_SERVER_BIND", address.to_string())
        .env("WWC_SERVER_PUBLIC_URL", format!("https://{address}"))
        .env("WWC_SERVER_ALLOWED_ORIGINS", format!("https://{address}"))
        .env("WWC_SERVER_DATA_DIRECTORY", directory.join("server"))
        .env("WWC_SERVER_REPOSITORY_ROOT", repository)
        .env("WWC_SERVER_AUTH_MODE", "password")
        .env("WWC_SERVER_BOOTSTRAP_PROOF", "bootstrap-proof")
        .env("WWC_SERVER_TLS_CERTIFICATE", certificate)
        .env("WWC_SERVER_TLS_PRIVATE_KEY", key)
        .env_remove("WWC_SERVER_TLS_PROXY")
        .env_remove("WWC_SERVER_PREVIEW_PUBLIC_URL")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut fixture = Fixture { directory, server };
    let body = json!({"schemaVersion":"winwincode/v1","username":"Owner","password":"initial-owner-password"});
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let logs = std::fs::read_to_string(&log).unwrap();
        assert!(
            !logs.contains("Could not automatically determine"),
            "{logs}"
        );
        assert!(fixture.server.try_wait().unwrap().is_none(), "{logs}");
        if let Ok((status, _)) = https_post(address, &client, "/api/v1/auth/session", &body, true) {
            assert_eq!(status, 201, "{logs}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "HTTPS bootstrap did not start: {logs}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    loop {
        if let Ok((201, session)) =
            https_post(address, &client, "/api/v1/auth/session", &body, false)
        {
            assert!(session.is_object());
            break;
        }
        let logs = std::fs::read_to_string(&log).unwrap();
        assert!(fixture.server.try_wait().unwrap().is_none(), "{logs}");
        assert!(
            Instant::now() < deadline,
            "HTTPS product did not start: {logs}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
