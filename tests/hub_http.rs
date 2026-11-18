use pigeonmq::hub_http;
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn fixture(reply: impl FnOnce(TcpStream) + Send + 'static) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "fixture did not receive request");
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("fixture accept: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "POST /receiver HTTP/1.1\r\n");
        let mut length = None;
        let mut headers = Vec::new();
        loop {
            line.clear();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            let (name, value) = line.trim_end().split_once(':').unwrap();
            headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
            if name.eq_ignore_ascii_case("content-length") {
                assert!(length.is_none());
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let value = |name| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .unwrap()
                .1
                .clone()
        };
        assert_eq!(value("host"), format!("127.0.0.1:{port}"));
        assert_eq!(value("authorization"), "Bearer fixture-token");
        assert_eq!(value("content-type"), "application/json");
        assert_eq!(value("accept"), "application/json, text/event-stream");
        assert_eq!(value("connection"), "close");
        assert!(!headers.iter().any(|(name, _)| name == "transfer-encoding"));
        let mut body = vec![0; length.unwrap()];
        reader.read_exact(&mut body).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"operation":"fixture"})
        );
        reply(reader.into_inner());
    });
    (port, worker)
}

fn call(port: u16) -> io::Result<(u16, Value)> {
    hub_http::post(
        port,
        "/receiver",
        "fixture-token",
        &json!({"operation":"fixture"}),
    )
}

fn exchange(response: &'static [u8]) -> io::Result<(u16, Value)> {
    let (port, worker) = fixture(move |mut stream| stream.write_all(response).unwrap());
    let result = call(port);
    worker.join().unwrap();
    result
}

#[test]
fn preserves_status_and_request_headers() {
    assert_eq!(
        exchange(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 4\r\n\r\nnull").unwrap(),
        (403, Value::Null)
    );
    assert_eq!(
        exchange(b"HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 4\r\n\r\nnull")
            .unwrap(),
        (302, Value::Null)
    );
}

#[test]
fn accepts_complete_content_length_without_waiting_for_eof() {
    let (release, wait) = mpsc::channel();
    let (port, worker) = fixture(move |mut stream| {
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nnull")
            .unwrap();
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
    });
    let result = call(port);
    release.send(()).unwrap();
    worker.join().unwrap();
    assert_eq!(result.unwrap(), (200, Value::Null));
}

#[test]
fn handles_standard_chunked_and_close_delimited_bodies() {
    assert_eq!(
        exchange(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nnull\r\n0\r\n\r\n")
            .unwrap(),
        (200, Value::Null)
    );
    assert_eq!(
        exchange(b"HTTP/1.0 200 OK\r\n\r\nnull").unwrap(),
        (200, Value::Null)
    );
}

#[test]
fn distinguishes_invalid_json_from_incomplete_http_body() {
    assert_eq!(
        exchange(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nnope")
            .unwrap_err()
            .to_string(),
        "bad_http_json"
    );
    assert_eq!(
        exchange(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nnull")
            .unwrap_err()
            .to_string(),
        "bad_http_response"
    );
}

#[test]
fn accepts_body_larger_than_former_one_mib_cap() {
    let payload_len = 1024 * 1024 + 1;
    let body = format!("\"{}\"", "x".repeat(payload_len));
    let (port, worker) = fixture(move |mut stream| {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body.as_bytes()).unwrap();
    });
    let result = call(port);
    worker.join().unwrap();
    assert_eq!(result.unwrap().1.as_str().unwrap().len(), payload_len);
}

#[test]
fn disconnected_request_is_not_replayed() {
    let (release, wait) = mpsc::channel();
    let (port, worker) = fixture(move |stream| {
        drop(stream);
        // Keep the listener open, without answering another connection.
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
    });
    let result = hub_http::post_until(
        port,
        "/receiver",
        "fixture-token",
        &json!({"operation":"fixture"}),
        Instant::now() + Duration::from_millis(500),
    );
    release.send(()).unwrap();
    worker.join().unwrap();
    assert_eq!(result.unwrap_err().to_string(), "bad_http_response");
}

#[test]
fn deadline_covers_both_response_headers_and_body() {
    for head in [
        b"".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n".as_slice(),
    ] {
        let (release, wait) = mpsc::channel();
        let (port, worker) = fixture(move |mut stream| {
            stream.write_all(head).unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
        });
        let started = Instant::now();
        let result = hub_http::post_until(
            port,
            "/receiver",
            "fixture-token",
            &json!({"operation":"fixture"}),
            started + Duration::from_millis(100),
        );
        release.send(()).unwrap();
        worker.join().unwrap();
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "hub_deadline_exceeded");
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}

#[test]
fn expired_deadline_does_not_connect() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let error = hub_http::post_until(
        listener.local_addr().unwrap().port(),
        "/receiver",
        "fixture-token",
        &Value::Null,
        Instant::now(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn synchronous_client_works_inside_tokio_current_thread_runtime() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            assert_eq!(
                exchange(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nnull").unwrap(),
                (200, Value::Null)
            );
        });
}

#[test]
fn missing_origin_form_is_rejected_before_connect() {
    let error = hub_http::post(1, "receiver", "fixture-token", &Value::Null).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(error.to_string(), "invalid_hub_path");
}
