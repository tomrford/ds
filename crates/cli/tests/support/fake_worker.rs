use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

pub fn create_server<F>(mut handle: F) -> (String, thread::JoinHandle<Vec<String>>)
where
    F: FnMut(usize, &str, &mut TcpStream) -> bool + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        let mut deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return requests;
                    }
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("failed to accept test HTTP connection: {error}"),
            };
            stream.set_nonblocking(false).unwrap();
            let request = read_http_request(&mut stream);
            let done = handle(requests.len(), &request, &mut stream);
            requests.push(request);
            if done {
                return requests;
            }
            deadline = Instant::now() + Duration::from_secs(2);
        }
    });
    (address, server)
}

pub fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0; 16 * 1024];
    let expected_length = loop {
        let read = stream.read(&mut buffer).unwrap();
        assert_ne!(read, 0, "HTTP request ended before its headers");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            break header_end + 4 + content_length;
        }
    };
    while bytes.len() < expected_length {
        let read = stream.read(&mut buffer).unwrap();
        assert_ne!(read, 0, "HTTP request ended before its body");
        bytes.extend_from_slice(&buffer[..read]);
    }
    String::from_utf8_lossy(&bytes[..expected_length]).into_owned()
}

pub fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    respond_bytes(stream, status, "application/json", body.as_bytes());
}

pub fn respond_bytes(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
}

pub fn repository_response(name: &str) -> String {
    format!(
        r#"{{"name":"{name}","repositoryId":"{}","incarnation":"{}"}}"#,
        "ab".repeat(32),
        "cd".repeat(16)
    )
}
