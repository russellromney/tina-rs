pub(crate) mod tina_impl;
pub(crate) mod tokio_impl;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SideReport {
    pub successful_get: u32,
    pub successful_post: u32,
    pub final_counter_value: u32,
    pub got_404_for_missing: bool,
    pub exit_clean: bool,
}

pub(crate) fn print_report(side: &str, report: &SideReport) {
    println!(
        "side={side} successful_get={} successful_post={} \
         final_counter_value={} got_404_for_missing={} exit_clean={}",
        report.successful_get,
        report.successful_post,
        report.final_counter_value,
        report.got_404_for_missing,
        report.exit_clean,
    );
}

pub(crate) fn assert_equivalent(tokio: &SideReport, tina: &SideReport) {
    // Both servers ran the same scripted client: 1 GET (returns 0), 3
    // POSTs (return 1, 2, 3), 1 GET (returns 3), 1 GET /missing (404).
    assert_eq!(tokio.successful_get, 2, "tokio served two GETs");
    assert_eq!(tina.successful_get, 2, "tina served two GETs");
    assert_eq!(tokio.successful_post, 3, "tokio served three POSTs");
    assert_eq!(tina.successful_post, 3, "tina served three POSTs");
    assert_eq!(
        tokio.final_counter_value, 3,
        "tokio counter ended at 3 after three POSTs"
    );
    assert_eq!(
        tina.final_counter_value, 3,
        "tina counter ended at 3 after three POSTs"
    );
    assert!(
        tokio.got_404_for_missing,
        "tokio returned 404 for unknown path"
    );
    assert!(
        tina.got_404_for_missing,
        "tina returned 404 for unknown path"
    );
    assert!(tokio.exit_clean, "tokio exited cleanly");
    assert!(tina.exit_clean, "tina exited cleanly");
}

/// The shared scripted client. Used by both sides; given a server
/// address, returns a [`SideReport`] of what the server did.
pub(crate) fn scripted_client(addr: std::net::SocketAddr) -> SideReport {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    fn one_request(addr: std::net::SocketAddr, request: &[u8]) -> Vec<u8> {
        let mut stream =
            TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect to server");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        stream.write_all(request).expect("write request");
        stream.flush().expect("flush");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("read response");
        response
    }

    fn parse_status_and_body(response: &[u8]) -> (u16, String) {
        let separator = b"\r\n\r\n";
        let header_end = response
            .windows(separator.len())
            .position(|w| w == separator)
            .expect("response has CRLFCRLF");
        let head = std::str::from_utf8(&response[..header_end]).expect("ASCII headers");
        // Status line: "HTTP/1.1 200 OK"
        let line = head.lines().next().expect("at least a status line");
        let mut parts = line.split_whitespace();
        let _version = parts.next();
        let status: u16 = parts
            .next()
            .expect("status code")
            .parse()
            .expect("status is u16");
        let body = std::str::from_utf8(&response[header_end + separator.len()..])
            .unwrap_or("")
            .to_owned();
        (status, body)
    }

    let mut report = SideReport {
        successful_get: 0,
        successful_post: 0,
        final_counter_value: 0,
        got_404_for_missing: false,
        exit_clean: true,
    };

    // Sequence: GET, POST, POST, POST, GET, GET /missing.
    let r1 = one_request(
        addr,
        b"GET /counter HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let (s1, b1) = parse_status_and_body(&r1);
    if s1 == 200 {
        report.successful_get += 1;
    }
    assert_eq!(b1.trim(), "0", "first GET should report counter=0");

    for _ in 0..3 {
        let r = one_request(
            addr,
            b"POST /counter HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let (status, _) = parse_status_and_body(&r);
        if status == 200 {
            report.successful_post += 1;
        }
    }

    let r2 = one_request(
        addr,
        b"GET /counter HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let (s2, b2) = parse_status_and_body(&r2);
    if s2 == 200 {
        report.successful_get += 1;
    }
    report.final_counter_value = b2.trim().parse().expect("counter is u32");

    let r3 = one_request(
        addr,
        b"GET /missing HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let (s3, _) = parse_status_and_body(&r3);
    if s3 == 404 {
        report.got_404_for_missing = true;
    }

    report
}
