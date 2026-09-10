use super::*;
use std::cell::Cell;

fn manifest(bytes: &[u8]) -> UpdaterVersionInfo {
    serde_json::from_value(serde_json::json!({
        "version": "0.4.9", "build_id": "a".repeat(40),
        "download_url": "https://github.com/chenjicheng/upmc/releases/download/v0.4.9/updater.exe",
        "size": bytes.len(), "sha256": format!("{:x}", Sha256::digest(bytes)),
    }))
    .unwrap()
}

#[test]
fn transfer_oversized_response_never_reads_or_writes_unbounded_bytes() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("updater.exe.new");
    let bytes = b"MZvalid fixture";
    let data = vec![b'x'; 1_000_000];
    let mut reader = std::io::Cursor::new(data);
    assert!(stage_download_reader(&mut reader, &path, &manifest(bytes), &|_| {}).is_err());
    assert!(
        reader.position() <= bytes.len() as u64 + 1,
        "read {} bytes despite declared size {}",
        reader.position(),
        bytes.len()
    );
    assert!(!path.exists(), "failed transfer retained partial staging");
}

#[test]
fn transfer_existing_staging_is_not_overwritten_or_removed() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("updater.exe.new");
    fs::write(&path, b"existing owner").unwrap();
    let bytes = b"MZvalid fixture";
    assert!(stage_download_reader(bytes.as_slice(), &path, &manifest(bytes), &|_| {}).is_err());
    assert_eq!(fs::read(path).unwrap(), b"existing owner");
}

#[test]
fn transfer_truncation_hash_mismatch_and_non_pe_leave_no_staged_file() {
    for (name, expected, actual) in [
        (
            "truncated",
            b"MZvalid fixture".as_slice(),
            b"MZshort".as_slice(),
        ),
        (
            "digest",
            b"MZvalid fixture".as_slice(),
            b"MZwrong fixture".as_slice(),
        ),
        (
            "non-pe",
            b"invalid fixture".as_slice(),
            b"invalid fixture".as_slice(),
        ),
    ] {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("updater.exe.new");
        assert!(
            stage_download_reader(actual, &path, &manifest(expected), &|_| {}).is_err(),
            "{name}"
        );
        assert!(!path.exists(), "{name} retained rejected bytes");
    }
}

#[test]
fn transfer_verified_bytes_are_staged_and_progress_is_bounded() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("updater.exe.new");
    let bytes = b"MZvalid fixture";
    stage_download_reader(bytes.as_slice(), &path, &manifest(bytes), &|p| {
        assert!((2..=10).contains(&p.percent))
    })
    .unwrap();
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn transfer_permanent_errors_are_not_retried() {
    for code in [400, 401, 403, 404, 501] {
        let calls = Cell::new(0);
        let sleeps = Cell::new(0);
        let error = bridge_retry_with::<()>(
            3,
            Duration::ZERO,
            "manifest",
            || {
                calls.set(calls.get() + 1);
                Err(ureq::Error::StatusCode(code).into())
            },
            |_| sleeps.set(sleeps.get() + 1),
        )
        .unwrap_err();
        assert!(error.downcast_ref::<ureq::Error>().is_some());
        assert_eq!(calls.get(), 1, "retried permanent HTTP {code}");
        assert_eq!(sleeps.get(), 0);
    }
    let calls = Cell::new(0);
    let error = bridge_retry_with::<()>(
        3,
        Duration::ZERO,
        "artifact",
        || {
            calls.set(calls.get() + 1);
            bail!("SHA256 mismatch")
        },
        |_| {},
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("SHA256 mismatch"));
    assert_eq!(calls.get(), 1, "retried integrity rejection");
}

#[test]
fn transfer_transient_failures_have_finite_attempts_and_no_final_sleep() {
    let calls = Cell::new(0);
    let sleeps = Cell::new(0);
    let error = bridge_retry_with::<()>(
        3,
        Duration::ZERO,
        "manifest",
        || {
            calls.set(calls.get() + 1);
            Err(ureq::Error::StatusCode(503).into())
        },
        |_| sleeps.set(sleeps.get() + 1),
    )
    .unwrap_err();
    assert_eq!(calls.get(), 3);
    assert_eq!(sleeps.get(), 2);
    assert!(error.downcast_ref::<ureq::Error>().is_some());
    assert!(format!("{error:#}").contains("3"));
}

#[test]
fn transfer_zero_budget_performs_no_operation() {
    assert!(
        bridge_retry_with::<()>(
            0,
            Duration::ZERO,
            "manifest",
            || panic!("zero budget operation"),
            |_| panic!("zero budget sleep")
        )
        .is_err()
    );
}

fn http_fixture(
    responses: Vec<Vec<u8>>,
    response_delay: Duration,
) -> (String, thread::JoinHandle<Vec<String>>) {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let worker = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut requests = Vec::new();
        for response in responses {
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "HTTP fixture accept deadline exhausted"
                        );
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => panic!("HTTP fixture accept: {e}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            requests.push(String::from_utf8(request).unwrap());
            thread::sleep(response_delay);
            // Timeout fixtures deliberately close their client before this write.
            let _ = socket.write_all(&response);
        }
        requests
    });
    (format!("http://{address}/bridge/dev/version.json"), worker)
}

fn response(status: &str, body: &[u8], declared_size: usize) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {declared_size}\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn fixture_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .proxy(None)
        .timeout_global(Some(timeout))
        .build()
        .into()
}

struct TimeoutFixture {
    url: String,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<thread::JoinHandle<std::result::Result<Vec<String>, String>>>,
}

impl TimeoutFixture {
    fn start() -> Self {
        use std::sync::{Arc, atomic::AtomicBool};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let worker = thread::spawn(move || Self::serve(listener, &stop).map_err(|e| e.to_string()));
        Self {
            url: format!("http://{address}/bridge/dev/version.json"),
            cancelled,
            worker: Some(worker),
        }
    }

    fn serve(
        listener: std::net::TcpListener,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> std::io::Result<Vec<String>> {
        use std::{io::ErrorKind, sync::atomic::Ordering};
        let mut requests = Vec::new();
        let mut socket = loop {
            if cancelled.load(Ordering::Acquire) {
                return Ok(requests);
            }
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2))
                }
                Err(e) => return Err(e),
            }
        };
        socket.set_nonblocking(true)?;
        let mut request = Vec::new();
        let mut buffer = [0; 512];
        while !request.ends_with(b"\r\n\r\n") {
            if cancelled.load(Ordering::Acquire) {
                return Ok(requests);
            }
            match socket.read(&mut buffer) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "client closed before request headers",
                    ));
                }
                Ok(count) => request.extend_from_slice(&buffer[..count]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2))
                }
                Err(e) => return Err(e),
            }
            if request.len() > 16 * 1024 {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "fixture request headers too large",
                ));
            }
        }
        requests.push(
            String::from_utf8(request)
                .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?,
        );
        socket.set_nonblocking(false)?;
        socket.set_write_timeout(Some(Duration::from_millis(500)))?;
        // Headers establish that the request was accepted before the body-read
        // timeout begins. Never race that timeout against a response sleep.
        socket.write_all(&response("200 OK", b"", 2))?;
        while !cancelled.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(2));
        }
        Ok(requests)
    }

    fn stop(&mut self) -> std::result::Result<Vec<String>, String> {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        let worker = self.worker.take().ok_or("fixture already stopped")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !worker.is_finished() {
            if std::time::Instant::now() >= deadline {
                return Err("timeout fixture did not stop within two seconds".into());
            }
            thread::sleep(Duration::from_millis(1));
        }
        worker
            .join()
            .map_err(|_| "timeout fixture worker panicked".to_owned())?
    }

    fn finish(mut self) -> std::result::Result<Vec<String>, String> {
        self.stop()
    }
}

impl Drop for TimeoutFixture {
    fn drop(&mut self) {
        if self.worker.is_some() {
            // Also cancel on early return or assertion unwind. Tests explicitly
            // finish before assertions so cleanup failures remain test failures.
            let _ = self.stop();
        }
    }
}

#[test]
fn transfer_timeout_fixture_cleans_up_after_preconnect_timeout() {
    let fixture = TimeoutFixture::start();
    let error = fetch_updater_info_from(&fixture_agent(Duration::ZERO), &fixture.url).unwrap_err();
    let cleanup = fixture.finish();
    assert!(
        matches!(
            error.downcast_ref::<ureq::Error>(),
            Some(ureq::Error::Timeout(ureq::Timeout::Global))
        ),
        "{error:#}"
    );
    assert!(
        cleanup.unwrap().is_empty(),
        "zero-budget client should not make a request"
    );
}

#[test]
fn transfer_timeout_fixture_cancels_before_delayed_client_starts() {
    let fixture = TimeoutFixture::start();
    // The client is deliberately not scheduled until cleanup has completed.
    // No sleep or tiny networking deadline determines this ordering.
    let (start, gate) = std::sync::mpsc::channel::<()>();
    let client = thread::spawn(move || gate.recv().is_ok());
    let cleanup = fixture.finish();
    drop(start);
    assert!(!client.join().unwrap());
    assert!(cleanup.unwrap().is_empty());
}

#[test]
fn transfer_real_http_retry_preserves_endpoint_and_parses_full_manifest() {
    let body = serde_json::to_vec(&serde_json::json!({"version":"0.4.8","build_id":"a".repeat(40),"size":15,"sha256":"b".repeat(64),"download_url":"https://gh.chenjicheng.cn/https://github.com/chenjicheng/upmc/releases/download/v0.4.8/updater.exe"})).unwrap();
    let (url, worker) = http_fixture(
        vec![
            response("503 Unavailable", b"", 0),
            response("200 OK", &body, body.len()),
        ],
        Duration::ZERO,
    );
    let info = bridge_retry_with(
        3,
        Duration::ZERO,
        &url,
        || fetch_updater_info_from(&fixture_agent(Duration::from_secs(1)), &url),
        |_| {},
    )
    .unwrap();
    assert!(
        !bridge_update_required(&info, "0.4.8", Some(&"a".repeat(40)), UpdateChannel::Dev).unwrap()
    );
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.starts_with("GET /bridge/dev/version.json HTTP/1.1\r\n"))
    );
}

fn assert_body_timeout(fixture: TimeoutFixture) {
    let agent = ureq::Agent::config_builder()
        .proxy(None)
        .timeout_global(Some(Duration::from_secs(5)))
        .timeout_recv_body(Some(Duration::from_millis(40)))
        .build()
        .into();
    let start = std::time::Instant::now();
    let result = fetch_updater_info_from(&agent, &fixture.url);
    let elapsed = start.elapsed();
    let cleanup = fixture.finish();
    let error = result.unwrap_err();
    assert!(elapsed < Duration::from_secs(5));
    assert!(
        matches!(
            error.downcast_ref::<ureq::Error>(),
            Some(ureq::Error::Timeout(ureq::Timeout::RecvBody))
        ),
        "expected body timeout, not a connect/global timeout or parse error: {error:#}"
    );
    assert!(
        is_transient_network_error(&error),
        "typed body timeout must remain retryable"
    );
    let requests = cleanup.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /bridge/dev/version.json HTTP/1.1\r\n"));
}

#[test]
fn transfer_real_http_timeout_is_typed_and_bounded() {
    assert_body_timeout(TimeoutFixture::start());
}

#[test]
fn transfer_timeout_fixture_delayed_client_still_tests_body_phase() {
    let fixture = TimeoutFixture::start();
    let (start, gate) = std::sync::mpsc::channel();
    let client = thread::spawn(move || {
        gate.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_body_timeout(fixture);
    });
    // Complete another fixture while the real HTTP client is explicitly held
    // back, rather than depending on a sleep winning a scheduling race.
    let cleanup = TimeoutFixture::start().finish();
    let released = start.send(());
    let client_result = client.join();
    assert!(cleanup.unwrap().is_empty());
    released.unwrap();
    client_result.unwrap();
}

#[test]
fn transfer_timeout_fixture_cancels_partial_request() {
    let fixture = TimeoutFixture::start();
    let address = fixture
        .url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    let mut socket = std::net::TcpStream::connect(address).unwrap();
    socket
        .write_all(b"GET /bridge/dev/version.json HTTP/1.1\r\n")
        .unwrap();
    assert!(fixture.finish().unwrap().is_empty());
}

#[test]
fn transfer_timeout_fixture_unwind_releases_listener() {
    let fixture = TimeoutFixture::start();
    let address = fixture
        .url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_owned();
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _fixture = fixture;
        std::panic::resume_unwind(Box::new("deliberate fixture assertion unwind"));
    }));
    assert!(unwind.is_err());
    // Rebinding the exact endpoint verifies the worker actually released its
    // listener during Drop, rather than merely setting a cancellation flag.
    let _rebound = std::net::TcpListener::bind(address).unwrap();
}

#[test]
fn transfer_timeout_fixture_stress_concurrent_early_and_body_timeouts() {
    thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..4 {
                    transfer_timeout_fixture_cleans_up_after_preconnect_timeout();
                    assert_body_timeout(TimeoutFixture::start());
                }
            });
        }
    });
}

#[test]
fn transfer_real_truncated_http_body_is_rejected_and_removed() {
    let expected = b"MZcomplete fixture";
    let (url, worker) = http_fixture(
        vec![response("200 OK", b"MZshort", expected.len())],
        Duration::ZERO,
    );
    let response = fixture_agent(Duration::from_secs(1))
        .get(&url)
        .call()
        .unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let staged = dir.path().join("updater.exe.new");
    assert!(
        stage_download_reader(
            response.into_body().into_reader(),
            &staged,
            &manifest(expected),
            &|_| {}
        )
        .is_err()
    );
    assert!(!staged.exists());
    assert_eq!(worker.join().unwrap().len(), 1);
}

#[test]
fn transfer_production_transport_rejects_http_before_request() {
    let error = bridge_http_agent(Duration::from_millis(10))
        .get("http://127.0.0.1:1/should-not-connect")
        .call()
        .unwrap_err();
    assert!(matches!(error, ureq::Error::RequireHttpsOnly(_)));
}

#[test]
fn transfer_permanent_metadata_error_emits_one_structured_failure() {
    crate::observability::take_events();
    let error = bridge_retry_with::<()>(
        3,
        Duration::ZERO,
        "bridge manifest",
        || bail!("required sha256 is missing"),
        |_| panic!("invalid metadata retried"),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("required sha256 is missing"));
    let events = crate::observability::take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["identifier"], "selfupdate.network.failed");
    assert_eq!(events[0]["level"], "ERROR");
    assert_eq!(events[0]["context_id"], "attempt 1/3");
    for key in ["reason", "primary_path", "fallback_path", "original_error"] {
        assert!(!events[0][key].as_str().unwrap().is_empty());
    }
    assert!(events[0]["counter"].as_u64().unwrap() > 0);
}
