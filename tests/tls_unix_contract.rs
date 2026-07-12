use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener as StdTcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::rsa::Rsa;
use openssl::ssl::{SslAcceptor, SslMethod, SslVerifyMode};
use openssl::symm::Cipher;
use openssl::x509::extension::{
    AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName,
    SubjectKeyIdentifier,
};
use openssl::x509::{X509NameBuilder, X509};
use serial_test::serial;

const IO_TIMEOUT: Duration = Duration::from_secs(3);
const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(10);
const ACTIVE_DRAIN_HOLD: Duration = Duration::from_millis(1_500);

fn reserve_port() -> u16 {
    StdTcpListener::bind(("127.0.0.1", 0))
        .expect("reserve TCP port")
        .local_addr()
        .expect("reserved TCP address")
        .port()
}

struct Binary {
    child: Child,
}

impl Binary {
    fn spawn(arguments: &[String]) -> Self {
        Self::spawn_with_env(arguments, &[])
    }

    fn spawn_with_env(arguments: &[String], environment: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pingora-reverse-proxy"));
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (name, value) in environment {
            command.env(name, value);
        }
        let child = command.spawn().expect("spawn proxy binary");
        Self { child }
    }

    fn assert_running(&mut self, diagnostic: &str) {
        assert!(
            self.child.try_wait().expect("poll proxy").is_none(),
            "{diagnostic}"
        );
    }

    fn wait_for_exit(&mut self) -> ExitStatus {
        self.wait_for_exit_within(IO_TIMEOUT)
    }

    fn wait_for_graceful_exit(&mut self) -> ExitStatus {
        self.wait_for_exit_within(PROCESS_EXIT_TIMEOUT)
    }

    fn wait_for_exit_within(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll proxy exit") {
                return status;
            }
            assert!(Instant::now() < deadline, "proxy exit timed out");
            std::thread::yield_now();
        }
    }
}

impl Drop for Binary {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn tcp_http(port: u16, request: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut stream = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => break stream,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "TCP listener {port} did not become ready: {error}"
                );
                std::thread::yield_now();
            }
        }
    };
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .expect("set TCP read timeout");
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .expect("set TCP write timeout");
    stream.write_all(request).expect("write TCP request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("read TCP response");
    response
}

fn assert_http(response: &[u8], status: &str, body: &[u8]) {
    let delimiter = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response delimiter");
    let headers = std::str::from_utf8(&response[..delimiter]).expect("HTTP headers are UTF-8");
    assert!(
        headers.starts_with(&format!("HTTP/1.1 {status}")),
        "unexpected response headers: {headers}"
    );
    assert_eq!(&response[delimiter + 4..], body);
}

fn signed_certificate(
    common_name: &str,
    key: &PKey<Private>,
    issuer: Option<(&X509, &PKey<Private>)>,
    server: bool,
) -> X509 {
    let mut name = X509NameBuilder::new().expect("certificate name builder");
    name.append_entry_by_text("CN", common_name)
        .expect("certificate common name");
    let name = name.build();
    let mut certificate = X509::builder().expect("certificate builder");
    certificate.set_version(2).expect("certificate version");
    let mut serial = BigNum::new().expect("serial number");
    serial
        .rand(64, MsbOption::MAYBE_ZERO, false)
        .expect("random serial");
    certificate
        .set_serial_number(&serial.to_asn1_integer().expect("ASN.1 serial"))
        .expect("certificate serial");
    certificate
        .set_subject_name(&name)
        .expect("certificate subject");
    certificate
        .set_issuer_name(issuer.map_or(&name, |(certificate, _)| certificate.subject_name()))
        .expect("certificate issuer");
    certificate.set_pubkey(key).expect("certificate key");
    certificate
        .set_not_before(&Asn1Time::days_from_now(0).expect("not before"))
        .expect("certificate not before");
    certificate
        .set_not_after(&Asn1Time::days_from_now(1).expect("not after"))
        .expect("certificate not after");
    if issuer.is_none() {
        certificate
            .append_extension(
                BasicConstraints::new()
                    .critical()
                    .ca()
                    .build()
                    .expect("CA constraints"),
            )
            .expect("append CA constraints");
        certificate
            .append_extension(
                KeyUsage::new()
                    .critical()
                    .key_cert_sign()
                    .crl_sign()
                    .build()
                    .expect("CA key usage"),
            )
            .expect("append CA key usage");
        let extension = {
            let context = certificate.x509v3_context(None, None);
            SubjectKeyIdentifier::new()
                .build(&context)
                .expect("CA subject key identifier")
        };
        certificate
            .append_extension(extension)
            .expect("append CA subject key identifier");
    } else {
        certificate
            .append_extension(
                BasicConstraints::new()
                    .critical()
                    .build()
                    .expect("leaf constraints"),
            )
            .expect("append leaf constraints");
        let extension = {
            let context = certificate
                .x509v3_context(issuer.map(|(certificate, _)| certificate.as_ref()), None);
            AuthorityKeyIdentifier::new()
                .keyid(true)
                .issuer(true)
                .build(&context)
                .expect("leaf authority key identifier")
        };
        certificate
            .append_extension(extension)
            .expect("append leaf authority key identifier");
        certificate
            .append_extension(
                KeyUsage::new()
                    .critical()
                    .digital_signature()
                    .key_encipherment()
                    .build()
                    .expect("leaf key usage"),
            )
            .expect("append leaf key usage");
        certificate
            .append_extension(if server {
                ExtendedKeyUsage::new()
                    .server_auth()
                    .build()
                    .expect("server key usage")
            } else {
                ExtendedKeyUsage::new()
                    .client_auth()
                    .build()
                    .expect("client key usage")
            })
            .expect("append extended key usage");
        if server {
            let extension = {
                let context = certificate
                    .x509v3_context(issuer.map(|(certificate, _)| certificate.as_ref()), None);
                SubjectAlternativeName::new()
                    .ip("127.0.0.1")
                    .dns("127.0.0.1")
                    .dns("localhost")
                    .build(&context)
                    .expect("server subject alternative name")
            };
            certificate
                .append_extension(extension)
                .expect("append server subject alternative name");
        }
    }
    certificate
        .sign(
            issuer.map_or(key, |(_, issuer_key)| issuer_key),
            MessageDigest::sha256(),
        )
        .expect("sign certificate");
    certificate.build()
}

struct TestPki {
    ca: X509,
    server: X509,
    server_key: PKey<Private>,
    ca_path: PathBuf,
    server_path: PathBuf,
    server_key_path: PathBuf,
    client_path: PathBuf,
    client_key_path: PathBuf,
    client_identity_path: PathBuf,
}

impl TestPki {
    fn new(directory: &tempfile::TempDir, encrypted_server_key: bool) -> Self {
        let ca_key = PKey::from_rsa(Rsa::generate(2048).expect("CA RSA key")).expect("CA key");
        let ca = signed_certificate("task-8-ca", &ca_key, None, false);
        let server_key =
            PKey::from_rsa(Rsa::generate(2048).expect("server RSA key")).expect("server key");
        let server = signed_certificate("localhost", &server_key, Some((&ca, &ca_key)), true);
        let client_key =
            PKey::from_rsa(Rsa::generate(2048).expect("client RSA key")).expect("client key");
        let client = signed_certificate("task-8-client", &client_key, Some((&ca, &ca_key)), false);
        let ca_path = directory.path().join("ca.pem");
        let server_path = directory.path().join("server.pem");
        let server_key_path = directory.path().join("server-key.pem");
        let client_path = directory.path().join("client.pem");
        let client_key_path = directory.path().join("client-key.pem");
        let client_identity_path = directory.path().join("client-identity.pem");
        fs::write(&ca_path, ca.to_pem().expect("CA PEM")).expect("write CA PEM");
        fs::write(&server_path, server.to_pem().expect("server PEM")).expect("write server PEM");
        let server_key_pem = if encrypted_server_key {
            server_key
                .private_key_to_pem_pkcs8_passphrase(Cipher::aes_256_cbc(), b"task-8-passphrase")
                .expect("encrypted server key PEM")
        } else {
            server_key
                .private_key_to_pem_pkcs8()
                .expect("server key PEM")
        };
        fs::write(&server_key_path, server_key_pem).expect("write server key PEM");
        let mut client_identity = client.to_pem().expect("client PEM");
        let client_key_pem = client_key
            .private_key_to_pem_pkcs8()
            .expect("client key PEM");
        fs::write(&client_path, client.to_pem().expect("client PEM")).expect("write client PEM");
        fs::write(&client_key_path, &client_key_pem).expect("write client key PEM");
        client_identity.extend_from_slice(&client_key_pem);
        fs::write(&client_identity_path, client_identity).expect("write client identity PEM");
        Self {
            ca,
            server,
            server_key,
            ca_path,
            server_path,
            server_key_path,
            client_path,
            client_key_path,
            client_identity_path,
        }
    }

    fn root_certificate(&self) -> reqwest::Certificate {
        reqwest::Certificate::from_pem(&self.ca.to_pem().expect("CA PEM"))
            .expect("reqwest root certificate")
    }

    fn client_identity(&self) -> reqwest::Identity {
        reqwest::Identity::from_pem(
            &fs::read(&self.client_identity_path).expect("client identity PEM"),
        )
        .expect("reqwest client identity")
    }
}

fn https_client(pki: &TestPki, identity: bool) -> reqwest::Client {
    let builder = reqwest::Client::builder()
        .timeout(IO_TIMEOUT)
        .add_root_certificate(pki.root_certificate());
    let builder = if identity {
        builder.identity(pki.client_identity())
    } else {
        builder
    };
    builder.build().expect("HTTPS client")
}

#[test]
#[serial]
fn public_api_and_metrics_tcp_listeners_serve_exact_contracts() {
    let public = reserve_port();
    let api = reserve_port();
    let metrics = reserve_port();
    let mut binary = Binary::spawn(&[
        "--ip".into(),
        "127.0.0.1".into(),
        "--port".into(),
        public.to_string(),
        "--api-ip".into(),
        "127.0.0.1".into(),
        "--api-port".into(),
        api.to_string(),
        "--metrics-ip".into(),
        "127.0.0.1".into(),
        "--metrics-port".into(),
        metrics.to_string(),
    ]);

    assert_http(
        &tcp_http(
            public,
            b"GET /_chp_healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        ),
        "200 OK",
        br#"{"status":"OK"}"#,
    );
    binary.assert_running("proxy exited after public readiness");
    assert_http(
        &tcp_http(
            api,
            b"GET /api/routes HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        ),
        "200 OK",
        b"{}",
    );
    assert_http(
        &tcp_http(
            metrics,
            b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        ),
        "200 OK",
        b"requests_api{status=\"200\"} 1\n",
    );
}

async fn wait_https_response(
    binary: &mut Binary,
    client: &reqwest::Client,
    url: &str,
    expected_body: &str,
) {
    let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
    loop {
        binary.assert_running("proxy exited before HTTPS readiness");
        if let Ok(response) = client.get(url).send().await {
            if response.status() == reqwest::StatusCode::OK
                && response.text().await.ok().as_deref() == Some(expected_body)
            {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "HTTPS endpoint did not reach exact readiness: {url}"
        );
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
#[serial]
async fn public_and_api_https_accept_encrypted_listener_keys() {
    let directory = tempfile::tempdir().expect("temporary TLS directory");
    let pki = TestPki::new(&directory, true);
    let public = reserve_port();
    let api = reserve_port();
    let mut binary = Binary::spawn_with_env(
        &[
            "--ip".into(),
            "127.0.0.1".into(),
            "--port".into(),
            public.to_string(),
            "--api-ip".into(),
            "127.0.0.1".into(),
            "--api-port".into(),
            api.to_string(),
            "--ssl-cert".into(),
            pki.server_path.display().to_string(),
            "--ssl-key".into(),
            pki.server_key_path.display().to_string(),
            "--api-ssl-cert".into(),
            pki.server_path.display().to_string(),
            "--api-ssl-key".into(),
            pki.server_key_path.display().to_string(),
        ],
        &[
            ("CONFIGPROXY_SSL_KEY_PASSPHRASE", "task-8-passphrase"),
            ("CONFIGPROXY_API_SSL_KEY_PASSPHRASE", "task-8-passphrase"),
        ],
    );
    let client = https_client(&pki, false);
    wait_https_response(
        &mut binary,
        &client,
        &format!("https://127.0.0.1:{public}/_chp_healthz"),
        r#"{"status":"OK"}"#,
    )
    .await;
    wait_https_response(
        &mut binary,
        &client,
        &format!("https://127.0.0.1:{api}/api/routes"),
        "{}",
    )
    .await;
}

#[tokio::test]
#[serial]
async fn stalled_api_tls_handshake_cannot_block_later_clients_indefinitely() {
    let directory = tempfile::tempdir().expect("temporary stalled TLS directory");
    let pki = TestPki::new(&directory, false);
    let public = reserve_port();
    let api = reserve_port();
    let mut binary = Binary::spawn(&[
        "--ip".into(),
        "127.0.0.1".into(),
        "--port".into(),
        public.to_string(),
        "--api-ip".into(),
        "127.0.0.1".into(),
        "--api-port".into(),
        api.to_string(),
        "--api-ssl-cert".into(),
        pki.server_path.display().to_string(),
        "--api-ssl-key".into(),
        pki.server_key_path.display().to_string(),
    ]);
    assert_http(
        &tcp_http(
            public,
            b"GET /_chp_healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        ),
        "200 OK",
        br#"{"status":"OK"}"#,
    );
    let stalled = TcpStream::connect(("127.0.0.1", api)).expect("open stalled TLS connection");
    let client = https_client(&pki, false);
    wait_https_response(
        &mut binary,
        &client,
        &format!("https://127.0.0.1:{api}/api/routes"),
        "{}",
    )
    .await;
    drop(stalled);
}

#[tokio::test]
#[serial]
async fn api_https_requires_a_client_certificate_when_configured() {
    let directory = tempfile::tempdir().expect("temporary mTLS directory");
    let pki = TestPki::new(&directory, false);
    let public = reserve_port();
    let api = reserve_port();
    let mut binary = Binary::spawn(&[
        "--ip".into(),
        "127.0.0.1".into(),
        "--port".into(),
        public.to_string(),
        "--api-ip".into(),
        "127.0.0.1".into(),
        "--api-port".into(),
        api.to_string(),
        "--api-ssl-cert".into(),
        pki.server_path.display().to_string(),
        "--api-ssl-key".into(),
        pki.server_key_path.display().to_string(),
        "--api-ssl-ca".into(),
        pki.ca_path.display().to_string(),
        "--api-ssl-request-cert".into(),
        "--api-ssl-reject-unauthorized".into(),
    ]);
    let url = format!("https://127.0.0.1:{api}/api/routes");
    let without_identity = https_client(&pki, false).get(&url).send().await;
    assert!(
        without_identity.is_err(),
        "API mTLS accepted a client without a certificate"
    );
    let client = https_client(&pki, true);
    wait_https_response(&mut binary, &client, &url, "{}").await;
}

fn mtls_upstream(pki: &TestPki, expect_success: bool) -> (u16, JoinHandle<()>) {
    let mut acceptor =
        SslAcceptor::mozilla_intermediate(SslMethod::tls()).expect("upstream TLS acceptor");
    acceptor
        .set_private_key(&pki.server_key)
        .expect("upstream private key");
    acceptor
        .set_certificate(&pki.server)
        .expect("upstream certificate");
    acceptor
        .cert_store_mut()
        .add_cert(pki.ca.clone())
        .expect("upstream client CA");
    acceptor.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);
    acceptor.check_private_key().expect("upstream identity");
    let acceptor = acceptor.build();
    let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("bind mTLS upstream");
    let port = listener.local_addr().expect("mTLS upstream address").port();
    let thread = std::thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .expect("nonblocking mTLS upstream");
        let deadline = Instant::now() + IO_TIMEOUT;
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "mTLS upstream accept timed out");
                    std::thread::yield_now();
                }
                Err(error) => panic!("mTLS upstream accept failed: {error}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("blocking mTLS upstream stream");
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .expect("mTLS upstream read timeout");
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .expect("mTLS upstream write timeout");
        let stream = acceptor.accept(stream);
        if !expect_success {
            assert!(
                stream.is_err(),
                "untrusted upstream TLS unexpectedly succeeded"
            );
            return;
        }
        let mut stream = stream.expect("mTLS upstream handshake");
        let mut request = [0; 4096];
        let count = stream.read(&mut request).expect("mTLS upstream request");
        assert!(
            std::str::from_utf8(&request[..count])
                .expect("mTLS upstream request UTF-8")
                .starts_with("GET /secure/verified HTTP/1.1"),
            "unexpected mTLS upstream request"
        );
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nmtls upstream",
            )
            .expect("mTLS upstream response");
    });
    (port, thread)
}

#[tokio::test]
#[serial]
async fn upstream_private_ca_and_client_certificate_are_applied() {
    let directory = tempfile::tempdir().expect("temporary upstream mTLS directory");
    let pki = TestPki::new(&directory, false);
    let (upstream, upstream_thread) = mtls_upstream(&pki, true);
    let public = reserve_port();
    let api = reserve_port();
    let mut binary = Binary::spawn(&[
        "--ip".into(),
        "127.0.0.1".into(),
        "--port".into(),
        public.to_string(),
        "--api-ip".into(),
        "127.0.0.1".into(),
        "--api-port".into(),
        api.to_string(),
        "--client-ssl-ca".into(),
        pki.ca_path.display().to_string(),
        "--client-ssl-cert".into(),
        pki.client_path.display().to_string(),
        "--client-ssl-key".into(),
        pki.client_key_path.display().to_string(),
    ]);
    let api_url = format!("http://127.0.0.1:{api}/api/routes/secure");
    let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
    loop {
        binary.assert_running("proxy exited before upstream mTLS route creation");
        if let Ok(response) = reqwest::Client::new()
            .post(&api_url)
            .json(&serde_json::json!({
                "target": format!("https://127.0.0.1:{upstream}")
            }))
            .send()
            .await
        {
            if response.status() == reqwest::StatusCode::CREATED {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "API did not become ready for upstream mTLS route"
        );
        tokio::task::yield_now().await;
    }
    let response = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{public}/secure/verified"))
        .send()
        .await
        .expect("proxy request to mTLS upstream");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.text().await.expect("mTLS upstream body"),
        "mtls upstream"
    );
    let deadline = Instant::now() + IO_TIMEOUT;
    while !upstream_thread.is_finished() {
        assert!(Instant::now() < deadline, "mTLS upstream thread timed out");
        std::thread::yield_now();
    }
    upstream_thread
        .join()
        .expect("mTLS upstream thread panicked");
}

#[tokio::test]
#[serial]
async fn upstream_private_ca_is_rejected_when_not_configured() {
    let directory = tempfile::tempdir().expect("temporary untrusted upstream directory");
    let pki = TestPki::new(&directory, false);
    let (upstream, upstream_thread) = mtls_upstream(&pki, false);
    let public = reserve_port();
    let api = reserve_port();
    let mut binary = Binary::spawn(&[
        "--ip".into(),
        "127.0.0.1".into(),
        "--port".into(),
        public.to_string(),
        "--api-ip".into(),
        "127.0.0.1".into(),
        "--api-port".into(),
        api.to_string(),
        "--client-ssl-cert".into(),
        pki.client_path.display().to_string(),
        "--client-ssl-key".into(),
        pki.client_key_path.display().to_string(),
    ]);
    let api_url = format!("http://127.0.0.1:{api}/api/routes/secure");
    let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
    loop {
        binary.assert_running("proxy exited before untrusted upstream route creation");
        if let Ok(response) = reqwest::Client::new()
            .post(&api_url)
            .json(&serde_json::json!({
                "target": format!("https://127.0.0.1:{upstream}")
            }))
            .send()
            .await
        {
            if response.status() == reqwest::StatusCode::CREATED {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "API did not become ready for untrusted upstream route"
        );
        tokio::task::yield_now().await;
    }
    let response = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{public}/secure/verified"))
        .send()
        .await
        .expect("proxy response for untrusted upstream");
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.text().await.expect("untrusted upstream body"),
        "Service Unavailable"
    );
    let deadline = Instant::now() + IO_TIMEOUT;
    while !upstream_thread.is_finished() {
        assert!(
            Instant::now() < deadline,
            "untrusted upstream thread timed out"
        );
        std::thread::yield_now();
    }
    upstream_thread
        .join()
        .expect("untrusted upstream thread panicked");
}

fn response_header(response: &[u8], name: &str) -> Option<String> {
    let delimiter = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?;
    std::str::from_utf8(&response[..delimiter])
        .ok()?
        .lines()
        .skip(1)
        .find_map(|line| {
            let (field, value) = line.split_once(':')?;
            field
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
}

#[tokio::test]
#[serial]
async fn redirect_without_host_is_400_and_host_uses_the_exact_https_port() {
    let directory = tempfile::tempdir().expect("temporary redirect TLS directory");
    let pki = TestPki::new(&directory, false);
    let public = reserve_port();
    let api = reserve_port();
    let redirect = reserve_port();
    let exact_https_port = reserve_port();
    let mut binary = Binary::spawn(&[
        "--ip".into(),
        "127.0.0.1".into(),
        "--port".into(),
        public.to_string(),
        "--api-ip".into(),
        "127.0.0.1".into(),
        "--api-port".into(),
        api.to_string(),
        "--ssl-cert".into(),
        pki.server_path.display().to_string(),
        "--ssl-key".into(),
        pki.server_key_path.display().to_string(),
        "--redirect-port".into(),
        redirect.to_string(),
        "--redirect-to".into(),
        exact_https_port.to_string(),
    ]);
    wait_https_response(
        &mut binary,
        &https_client(&pki, false),
        &format!("https://127.0.0.1:{public}/_chp_healthz"),
        r#"{"status":"OK"}"#,
    )
    .await;

    let no_host = tcp_http(
        redirect,
        b"GET /no-host?x=1 HTTP/1.0\r\nConnection: close\r\n\r\n",
    );
    let no_host_delimiter = no_host
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("no-Host redirect delimiter");
    assert!(std::str::from_utf8(&no_host[..no_host_delimiter])
        .expect("no-Host redirect headers")
        .starts_with("HTTP/1.0 400 Bad Request"));
    assert_eq!(&no_host[no_host_delimiter + 4..], b"");
    let redirected = tcp_http(
        redirect,
        b"GET /user/alice?x=%2F HTTP/1.1\r\nHost: hub.example:1234\r\nConnection: close\r\n\r\n",
    );
    assert_http(&redirected, "301 Moved Permanently", b"");
    assert_eq!(
        response_header(&redirected, "location").as_deref(),
        Some(format!("https://hub.example:{exact_https_port}/user/alice?x=%2F").as_str())
    );
}

#[test]
#[serial]
fn pid_guard_is_removed_when_startup_fails_after_atomic_creation() {
    let directory = tempfile::tempdir().expect("temporary PID failure directory");
    let pid_file = directory.path().join("proxy.pid");
    let missing_cert = directory.path().join("missing-cert.pem");
    let missing_key = directory.path().join("missing-key.pem");
    let mut binary = Binary::spawn(&[
        "--pid-file".into(),
        pid_file.display().to_string(),
        "--port".into(),
        reserve_port().to_string(),
        "--api-port".into(),
        reserve_port().to_string(),
        "--ssl-cert".into(),
        missing_cert.display().to_string(),
        "--ssl-key".into(),
        missing_key.display().to_string(),
    ]);
    assert!(
        !binary.wait_for_exit().success(),
        "invalid TLS startup succeeded"
    );
    assert!(
        !pid_file.exists(),
        "PID guard remained after startup returned an error"
    );
}

#[cfg(unix)]
#[test]
#[serial]
fn sigterm_allows_an_active_proxy_response_to_finish_before_runtime_termination() {
    use std::sync::mpsc;

    let upstream = StdTcpListener::bind(("127.0.0.1", 0)).expect("bind draining upstream");
    let upstream_address = upstream.local_addr().expect("draining upstream address");
    let (response_started_tx, response_started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let upstream_thread = std::thread::spawn(move || {
        let (mut stream, _) = upstream.accept().expect("draining upstream accept");
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .expect("draining upstream read timeout");
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .expect("draining upstream write timeout");
        let mut request = [0; 4096];
        let count = stream
            .read(&mut request)
            .expect("draining upstream request");
        assert!(count > 0, "draining upstream request was empty");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\ndra")
            .expect("start draining upstream response");
        response_started_tx
            .send(())
            .expect("signal started upstream response");
        release_rx
            .recv_timeout(IO_TIMEOUT)
            .expect("active request was not released");
        stream
            .write_all(b"ined")
            .expect("finish draining upstream response");
    });

    let public = reserve_port();
    let api = reserve_port();
    let mut binary = Binary::spawn(&[
        "--ip".into(),
        "127.0.0.1".into(),
        "--port".into(),
        public.to_string(),
        "--api-ip".into(),
        "127.0.0.1".into(),
        "--api-port".into(),
        api.to_string(),
        "--default-target".into(),
        format!("http://{upstream_address}"),
    ]);
    let routes = tcp_http(
        api,
        b"GET /api/routes HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    let delimiter = routes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("route readiness response delimiter");
    assert!(std::str::from_utf8(&routes[..delimiter])
        .expect("route readiness headers")
        .starts_with("HTTP/1.1 200 OK"));
    let routes: serde_json::Value =
        serde_json::from_slice(&routes[delimiter + 4..]).expect("route readiness JSON");
    assert_eq!(routes["/"]["target"], format!("http://{upstream_address}/"));
    assert_http(
        &tcp_http(
            public,
            b"GET /_chp_healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        ),
        "200 OK",
        br#"{"status":"OK"}"#,
    );
    let mut client = TcpStream::connect(("127.0.0.1", public)).expect("connect draining client");
    client
        .set_read_timeout(Some(IO_TIMEOUT))
        .expect("set draining client read timeout");
    client
        .set_write_timeout(Some(IO_TIMEOUT))
        .expect("set draining client write timeout");
    client
        .write_all(b"GET /drain HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write draining client request");
    response_started_rx
        .recv_timeout(IO_TIMEOUT)
        .expect("upstream response never started");
    unsafe {
        libc::kill(binary.child.id() as libc::pid_t, libc::SIGTERM);
    }
    let hold_deadline = Instant::now() + ACTIVE_DRAIN_HOLD;
    while Instant::now() < hold_deadline {
        binary.assert_running("proxy terminated an admitted active response");
        std::thread::yield_now();
    }
    release_tx.send(()).expect("release draining upstream");
    let mut response = Vec::new();
    client
        .read_to_end(&mut response)
        .expect("read completed draining response");
    assert_http(&response, "200 OK", b"drained");
    assert!(
        binary.wait_for_graceful_exit().success(),
        "SIGTERM drain did not exit cleanly"
    );
    upstream_thread
        .join()
        .expect("draining upstream thread panicked");
}

#[cfg(unix)]
fn unix_http(path: &Path, request: &[u8]) -> Vec<u8> {
    use std::os::unix::net::UnixStream;

    let deadline = Instant::now() + IO_TIMEOUT;
    let mut stream = loop {
        match UnixStream::connect(path) {
            Ok(stream) => break stream,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "Unix listener {} did not become ready: {error}",
                    path.display()
                );
                std::thread::yield_now();
            }
        }
    };
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .expect("set Unix read timeout");
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .expect("set Unix write timeout");
    stream.write_all(request).expect("write Unix request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("read Unix response");
    response
}

#[cfg(unix)]
#[test]
#[serial]
fn public_api_and_metrics_unix_sockets_serve_and_are_cleaned_up() {
    let directory = tempfile::tempdir().expect("temporary socket directory");
    let public = directory.path().join("public.sock");
    let api = directory.path().join("api.sock");
    let metrics = directory.path().join("metrics.sock");
    let mut binary = Binary::spawn(&[
        "--socket".into(),
        public.display().to_string(),
        "--api-socket".into(),
        api.display().to_string(),
        "--metrics-socket".into(),
        metrics.display().to_string(),
    ]);

    assert_http(
        &unix_http(
            &public,
            b"GET /_chp_healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        ),
        "200 OK",
        br#"{"status":"OK"}"#,
    );
    assert_http(
        &unix_http(
            &api,
            b"GET /api/routes HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        ),
        "200 OK",
        b"{}",
    );
    assert_http(
        &unix_http(
            &metrics,
            b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        ),
        "200 OK",
        b"requests_api{status=\"200\"} 1\n",
    );
    binary.assert_running("proxy exited after Unix requests");
    unsafe {
        libc::kill(binary.child.id() as libc::pid_t, libc::SIGTERM);
    }
    assert!(
        binary.wait_for_graceful_exit().success(),
        "SIGTERM was not graceful"
    );
    assert!(!public.exists(), "public Unix socket was not cleaned up");
    assert!(!api.exists(), "API Unix socket was not cleaned up");
    assert!(!metrics.exists(), "metrics Unix socket was not cleaned up");
}

#[cfg(unix)]
#[test]
#[serial]
fn sigterm_drains_and_removes_the_atomic_pid_guard() {
    let directory = tempfile::tempdir().expect("temporary PID directory");
    let pid_file = directory.path().join("proxy.pid");
    let public = reserve_port();
    let api = reserve_port();
    let mut binary = Binary::spawn(&[
        "--ip".into(),
        "127.0.0.1".into(),
        "--port".into(),
        public.to_string(),
        "--api-ip".into(),
        "127.0.0.1".into(),
        "--api-port".into(),
        api.to_string(),
        "--pid-file".into(),
        pid_file.display().to_string(),
    ]);
    assert_http(
        &tcp_http(
            api,
            b"GET /api/routes HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        ),
        "200 OK",
        b"{}",
    );
    let pid = fs::read_to_string(&pid_file).expect("PID file exists while running");
    assert_eq!(pid, format!("{}\n", binary.child.id()));

    unsafe {
        libc::kill(binary.child.id() as libc::pid_t, libc::SIGTERM);
    }
    assert!(
        binary.wait_for_graceful_exit().success(),
        "SIGTERM was not graceful"
    );
    assert!(
        !pid_file.exists(),
        "PID file remained after graceful shutdown"
    );
}

#[test]
#[serial]
fn existing_pid_or_socket_paths_are_refused_without_deleting_the_owner() {
    let directory = tempfile::tempdir().expect("temporary collision directory");
    let pid_file = directory.path().join("proxy.pid");
    fs::write(&pid_file, b"existing owner\n").expect("write existing PID file");
    let mut binary = Binary::spawn(&[
        "--pid-file".into(),
        pid_file.display().to_string(),
        "--port".into(),
        reserve_port().to_string(),
        "--api-port".into(),
        reserve_port().to_string(),
    ]);
    assert!(
        !binary.wait_for_exit().success(),
        "existing PID was accepted"
    );
    assert_eq!(
        fs::read_to_string(&pid_file).expect("existing PID retained"),
        "existing owner\n"
    );

    #[cfg(unix)]
    {
        let socket = directory.path().join("existing.sock");
        fs::write(&socket, b"owner data").expect("write colliding socket path");
        let mut binary = Binary::spawn(&[
            "--socket".into(),
            socket.display().to_string(),
            "--api-port".into(),
            reserve_port().to_string(),
        ]);
        assert!(
            !binary.wait_for_exit().success(),
            "existing socket path was accepted"
        );
        assert_eq!(
            fs::read(&socket).expect("socket owner retained"),
            b"owner data"
        );
    }
}

#[allow(dead_code)]
fn _pathbuf_type_is_part_of_the_collision_contract(_: PathBuf) {}
