use parking_lot::Mutex;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Shared with an encrypted transport so rustls checks the command deadline
/// between successful reads of a fragmented TLS record.
#[derive(Clone, Default)]
pub struct OperationDeadline(Arc<Mutex<Option<Instant>>>);

impl OperationDeadline {
    pub fn current(&self) -> Option<Instant> {
        *self.0.lock()
    }

    fn activate(&self, deadline: Instant) -> DeadlineGuard {
        *self.0.lock() = Some(deadline);
        DeadlineGuard(self.clone())
    }
}

struct DeadlineGuard(OperationDeadline);

impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        *self.0.0.lock() = None;
    }
}

const MAX_LINE: usize = 8 * 1024;
const MAX_VALUE: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_RECORDS: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseKind {
    Values { cas: bool },
    Stats,
    Meta,
    Simple,
    Version,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Item {
    pub key: Vec<u8>,
    pub flags: u32,
    pub cas: Option<u64>,
    pub value: Vec<u8>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum WireResponse {
    Values(Vec<Item>),
    Stats(Vec<(String, String)>),
    Meta {
        code: String,
        tokens: Vec<String>,
        value: Option<Vec<u8>>,
    },
    Status(String),
    Version(String),
}

/// The underlying stream must have finite read/write timeouts. For encrypted
/// streams, use `with_deadline` to enforce a deadline inside TLS record reads.
pub struct Wire<S: Read + Write> {
    stream: BufReader<S>,
    timeout: Duration,
    deadline: Option<OperationDeadline>,
}

impl<S: Read + Write> Wire<S> {
    pub fn new(stream: S, timeout: Duration) -> Self {
        Self {
            stream: BufReader::new(stream),
            timeout,
            deadline: None,
        }
    }

    pub fn with_deadline(stream: S, timeout: Duration, deadline: OperationDeadline) -> Self {
        Self {
            stream: BufReader::new(stream),
            timeout,
            deadline: Some(deadline),
        }
    }
    /// Sends the request once, then consumes one complete response. On any
    /// error the caller must discard this connection: its framing or the
    /// outcome of a partially sent mutation may be unknown.
    pub fn execute(
        &mut self,
        request: &[u8],
        kind: ResponseKind,
        cancel: &AtomicBool,
    ) -> io::Result<WireResponse> {
        let deadline = Instant::now().checked_add(self.timeout).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "command timeout too large")
        })?;
        let _deadline = self
            .deadline
            .as_ref()
            .map(|control| control.activate(deadline));
        let mut operation = Operation {
            stream: &mut self.stream,
            timeout: self.timeout,
            started: Instant::now(),
            cancel,
        };
        operation.write_request(request)?;
        let line = operation.read_line()?;
        let response = if is_server_error(&line) {
            WireResponse::Status(parse_text(&line)?.to_owned())
        } else {
            match kind {
                ResponseKind::Values { cas } => operation.read_values(line, cas)?,
                ResponseKind::Stats => operation.read_stats(line)?,
                ResponseKind::Meta => operation.read_meta(line)?,
                ResponseKind::Simple => parse_simple(&line)?,
                ResponseKind::Version => parse_version(&line)?,
            }
        };
        operation.check()?;
        Ok(response)
    }
}

struct Operation<'a, S: Read + Write> {
    stream: &'a mut BufReader<S>,
    timeout: Duration,
    started: Instant,
    cancel: &'a AtomicBool,
}

impl<S: Read + Write> Operation<'_, S> {
    fn check(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "command cancelled",
            ));
        }
        if self.started.elapsed() >= self.timeout {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
        }
        Ok(())
    }

    fn write_request(&mut self, request: &[u8]) -> io::Result<()> {
        let mut offset = 0;
        while offset < request.len() {
            self.check()?;
            match self.stream.get_mut().write(&request[offset..]) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(n) => offset += n,
                Err(error) if retryable(&error) => continue,
                Err(error) => return Err(error),
            }
        }
        loop {
            self.check()?;
            match self.stream.get_mut().flush() {
                Ok(()) => return Ok(()),
                Err(error) if retryable(&error) => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn read_line(&mut self) -> io::Result<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            self.check()?;
            match self.stream.fill_buf() {
                Ok([]) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                Ok(available) => {
                    let end = available.iter().position(|&byte| byte == b'\n');
                    let count = end.map_or(available.len(), |pos| pos + 1);
                    if line.len() + count > MAX_LINE {
                        return Err(invalid("response line exceeds 8 KiB"));
                    }
                    line.extend_from_slice(&available[..count]);
                    self.stream.consume(count);
                    if end.is_some() {
                        if !line.ends_with(b"\r\n") || line[..line.len() - 2].contains(&b'\r') {
                            return Err(invalid("response line is not CRLF-terminated"));
                        }
                        line.truncate(line.len() - 2);
                        return Ok(line);
                    }
                }
                Err(error) if retryable(&error) => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn read_exact_bytes(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let mut offset = 0;
        while offset < buf.len() {
            self.check()?;
            match self.stream.read(&mut buf[offset..]) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                Ok(n) => offset += n,
                Err(error) if retryable(&error) => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn read_body(&mut self, len: usize) -> io::Result<Vec<u8>> {
        if len > MAX_VALUE {
            return Err(invalid("value exceeds 16 MiB"));
        }
        let mut value = vec![0; len];
        self.read_exact_bytes(&mut value)?;
        let mut terminator = [0; 2];
        self.read_exact_bytes(&mut terminator)?;
        if terminator != *b"\r\n" {
            return Err(invalid("value lacks trailing CRLF"));
        }
        Ok(value)
    }

    fn read_values(&mut self, mut line: Vec<u8>, cas: bool) -> io::Result<WireResponse> {
        let mut values = Vec::new();
        let mut received = 0;
        loop {
            charge(&mut received, line.len() + 2)?;
            if line == b"END" {
                return Ok(WireResponse::Values(values));
            }
            if is_server_error(&line) {
                return Ok(WireResponse::Status(parse_text(&line)?.to_owned()));
            }
            if values.len() == MAX_RECORDS {
                return Err(invalid("too many value records"));
            }
            let fields = fields(&line)?;
            if fields.len() != if cas { 5 } else { 4 } || fields[0] != b"VALUE" {
                return Err(invalid("invalid VALUE header"));
            }
            let key = fields[1];
            if key.is_empty() || key.len() > 250 || key.iter().any(|&b| b <= b' ' || b == 127) {
                return Err(invalid("invalid VALUE key"));
            }
            let flags = decimal::<u32>(fields[2], "flags")?;
            let len = decimal::<usize>(fields[3], "value length")?;
            let cas = if cas {
                Some(decimal::<u64>(fields[4], "CAS")?)
            } else {
                None
            };
            if len > MAX_VALUE {
                return Err(invalid("value exceeds 16 MiB"));
            }
            charge(&mut received, len + 2)?;
            let value = self.read_body(len)?;
            values.push(Item {
                key: key.to_vec(),
                flags,
                cas,
                value,
            });
            line = self.read_line()?;
        }
    }

    fn read_stats(&mut self, mut line: Vec<u8>) -> io::Result<WireResponse> {
        let mut stats = Vec::new();
        let mut received = 0;
        loop {
            charge(&mut received, line.len() + 2)?;
            if line == b"END" {
                return Ok(WireResponse::Stats(stats));
            }
            if is_server_error(&line) {
                return Ok(WireResponse::Status(parse_text(&line)?.to_owned()));
            }
            if stats.len() == MAX_RECORDS {
                return Err(invalid("too many statistics"));
            }
            let record = line
                .strip_prefix(b"STAT ")
                .ok_or_else(|| invalid("invalid STAT line"))?;
            let separator = record
                .iter()
                .position(|&b| b == b' ')
                .ok_or_else(|| invalid("STAT lacks value"))?;
            if separator == 0 || separator + 1 == record.len() {
                return Err(invalid("STAT lacks name or value"));
            }
            let name = parse_text(&record[..separator])?;
            if name.bytes().any(|b| b <= b' ' || b == 127) {
                return Err(invalid("invalid STAT name"));
            }
            let value = parse_text(&record[separator + 1..])?;
            stats.push((name.to_owned(), value.to_owned()));
            line = self.read_line()?;
        }
    }

    fn read_meta(&mut self, line: Vec<u8>) -> io::Result<WireResponse> {
        let parts = fields(&line)?;
        let code = parts[0];
        let has_body = code == b"VA";
        if has_body && parts.len() < 2 {
            return Err(invalid("VA lacks value length"));
        }
        if ![
            b"VA".as_slice(),
            b"HD",
            b"EN",
            b"NF",
            b"NS",
            b"EX",
            b"MN",
            b"ME",
        ]
        .contains(&code)
        {
            return Err(invalid("unknown meta response code"));
        }
        if code == b"MN" && parts.len() != 1 {
            return Err(invalid("MN has unexpected tokens"));
        }
        let value = if has_body {
            let len = decimal::<usize>(parts[1], "meta value length")?;
            Some(self.read_body(len)?)
        } else {
            None
        };
        let start = if has_body { 2 } else { 1 };
        let tokens = parts[start..]
            .iter()
            .map(|part| parse_text(part).map(str::to_owned))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(WireResponse::Meta {
            code: parse_text(code)?.to_owned(),
            tokens,
            value,
        })
    }
}

fn charge(received: &mut usize, amount: usize) -> io::Result<()> {
    if amount > MAX_RESPONSE_BYTES - *received {
        return Err(invalid("response exceeds 32 MiB"));
    }
    *received += amount;
    Ok(())
}

fn retryable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
    )
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn parse_text(bytes: &[u8]) -> io::Result<&str> {
    std::str::from_utf8(bytes).map_err(|_| invalid("response is not UTF-8 text"))
}

fn decimal<T: std::str::FromStr>(bytes: &[u8], label: &'static str) -> io::Result<T> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(invalid(label));
    }
    parse_text(bytes)?.parse::<T>().map_err(|_| invalid(label))
}

fn fields(line: &[u8]) -> io::Result<Vec<&[u8]>> {
    let fields: Vec<_> = line.split(|&b| b == b' ').collect();
    if fields
        .iter()
        .any(|part| part.is_empty() || part.iter().any(u8::is_ascii_control))
    {
        return Err(invalid("malformed response tokens"));
    }
    Ok(fields)
}

fn is_server_error(line: &[u8]) -> bool {
    line == b"ERROR"
        || line == b"CLIENT_ERROR"
        || line.starts_with(b"CLIENT_ERROR ")
        || line == b"SERVER_ERROR"
        || line.starts_with(b"SERVER_ERROR ")
}

fn parse_simple(line: &[u8]) -> io::Result<WireResponse> {
    if [
        "STORED",
        "NOT_STORED",
        "EXISTS",
        "NOT_FOUND",
        "DELETED",
        "TOUCHED",
        "OK",
    ]
    .contains(&parse_text(line)?)
        || decimal::<u64>(line, "invalid simple response").is_ok()
    {
        return Ok(WireResponse::Status(parse_text(line)?.to_owned()));
    }
    Err(invalid("invalid simple response"))
}

fn parse_version(line: &[u8]) -> io::Result<WireResponse> {
    let version = line
        .strip_prefix(b"VERSION ")
        .ok_or_else(|| invalid("invalid VERSION response"))?;
    if version.is_empty() {
        return Err(invalid("empty VERSION response"));
    }
    Ok(WireResponse::Version(parse_text(version)?.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::rc::Rc;

    struct FakeStream {
        incoming: Cursor<Vec<u8>>,
        written: Rc<RefCell<Vec<u8>>>,
        max_write: usize,
        read_fault: Option<io::ErrorKind>,
        write_fault: Option<io::ErrorKind>,
    }

    impl Read for FakeStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if let Some(kind) = self.read_fault.take() {
                return Err(io::Error::from(kind));
            }
            self.incoming.read(buf)
        }
    }

    impl Write for FakeStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Some(kind) = self.write_fault.take() {
                return Err(io::Error::from(kind));
            }
            let count = buf.len().min(self.max_write);
            self.written.borrow_mut().extend_from_slice(&buf[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn transport(response: &[u8], max_write: usize) -> (Wire<FakeStream>, Rc<RefCell<Vec<u8>>>) {
        let written = Rc::new(RefCell::new(Vec::new()));
        (
            Wire::new(
                FakeStream {
                    incoming: Cursor::new(response.to_vec()),
                    written: written.clone(),
                    max_write,
                    read_fault: None,
                    write_fault: None,
                },
                Duration::from_secs(1),
            ),
            written,
        )
    }

    fn run(response: &[u8], kind: ResponseKind) -> io::Result<WireResponse> {
        let (mut wire, _) = transport(response, 1024);
        wire.execute(b"get key\r\n", kind, &AtomicBool::new(false))
    }

    #[test]
    fn retries_transient_read_and_write_failures_without_replaying_request() {
        let (mut wire, written) = transport(b"DELETED\r\n", 2);
        wire.stream.get_mut().read_fault = Some(io::ErrorKind::TimedOut);
        wire.stream.get_mut().write_fault = Some(io::ErrorKind::Interrupted);
        assert_eq!(
            wire.execute(
                b"delete key\r\n",
                ResponseKind::Simple,
                &AtomicBool::new(false)
            )
            .unwrap(),
            WireResponse::Status("DELETED".into())
        );
        assert_eq!(&*written.borrow(), b"delete key\r\n");
    }

    #[test]
    fn reads_multikey_binary_values_and_partial_writes() {
        let (mut wire, written) = transport(
            b"VALUE a 0 6\r\nx\r\ny\0z\r\nVALUE b 17 0\r\n\r\nEND\r\n",
            2,
        );
        assert_eq!(
            wire.execute(
                b"get a b\r\n",
                ResponseKind::Values { cas: false },
                &AtomicBool::new(false)
            )
            .unwrap(),
            WireResponse::Values(vec![
                Item {
                    key: b"a".to_vec(),
                    flags: 0,
                    cas: None,
                    value: b"x\r\ny\0z".to_vec()
                },
                Item {
                    key: b"b".to_vec(),
                    flags: 17,
                    cas: None,
                    value: vec![]
                },
            ])
        );
        assert_eq!(&*written.borrow(), b"get a b\r\n");
    }

    #[test]
    fn reads_cas_and_requires_end() {
        assert_eq!(
            run(
                b"VALUE key 2 1 99\r\nv\r\nEND\r\n",
                ResponseKind::Values { cas: true }
            )
            .unwrap(),
            WireResponse::Values(vec![Item {
                key: b"key".to_vec(),
                flags: 2,
                cas: Some(99),
                value: b"v".to_vec()
            }])
        );
        assert_eq!(
            run(
                b"VALUE key 0 1\r\nv\r\n",
                ResponseKind::Values { cas: false }
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn rejects_short_or_unterminated_binary_body() {
        assert_eq!(
            run(b"VALUE k 0 5\r\nabc", ResponseKind::Values { cas: false })
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            run(
                b"VALUE k 0 3\r\nabc!!END\r\n",
                ResponseKind::Values { cas: false }
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn enforces_framing_and_size_limits() {
        for response in [
            b"VALUE k 0 16777217\r\n".as_slice(),
            b"VALUE k 0 nope\r\n".as_slice(),
            b"VALUE k 0 0\n\r\nEND\r\n".as_slice(),
            b"VALUE k 0 0\r\n\r\nWRONG\r\n".as_slice(),
        ] {
            assert_eq!(
                run(response, ResponseKind::Values { cas: false })
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        let long_line = [vec![b'x'; MAX_LINE], b"\r\n".to_vec()].concat();
        assert_eq!(
            run(&long_line, ResponseKind::Simple).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn aggregate_multikey_response_cannot_exhaust_memory() {
        let value = vec![b'x'; 1024 * 1024];
        let mut response = Vec::with_capacity(33 * (value.len() + 24));
        for _ in 0..33 {
            response.extend_from_slice(b"VALUE k 0 1048576\r\n");
            response.extend_from_slice(&value);
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"END\r\n");
        assert!(
            matches!(run(&response, ResponseKind::Values { cas: false }), Err(error) if error.kind() == io::ErrorKind::InvalidData),
            "aggregate value bytes must have a finite response-wide limit"
        );
    }

    #[test]
    fn reads_statistics_through_end_and_version() {
        assert_eq!(
            run(
                b"STAT curr_items 3\r\nSTAT custom:field words here\r\nEND\r\n",
                ResponseKind::Stats
            )
            .unwrap(),
            WireResponse::Stats(vec![
                ("curr_items".into(), "3".into()),
                ("custom:field".into(), "words here".into())
            ])
        );
        assert_eq!(
            run(b"STAT curr_items 3\r\n", ResponseKind::Stats)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            run(b"VERSION 1.6.39\r\n", ResponseKind::Version).unwrap(),
            WireResponse::Version("1.6.39".into())
        );
    }

    #[test]
    fn preserves_meta_metadata_and_reads_va_body() {
        assert_eq!(
            run(
                b"VA 5 t-1 c42 X W zunknown\r\na\r\nbc\r\n",
                ResponseKind::Meta
            )
            .unwrap(),
            WireResponse::Meta {
                code: "VA".into(),
                tokens: vec![
                    "t-1".into(),
                    "c42".into(),
                    "X".into(),
                    "W".into(),
                    "zunknown".into()
                ],
                value: Some(b"a\r\nbc".to_vec())
            }
        );
        assert_eq!(
            run(b"ME key exp=99 cls=2\r\n", ResponseKind::Meta).unwrap(),
            WireResponse::Meta {
                code: "ME".into(),
                tokens: vec!["key".into(), "exp=99".into(), "cls=2".into()],
                value: None
            }
        );
        for code in ["HD", "EN", "NF", "NS", "EX", "MN"] {
            assert_eq!(
                run(format!("{code}\r\n").as_bytes(), ResponseKind::Meta).unwrap(),
                WireResponse::Meta {
                    code: code.into(),
                    tokens: vec![],
                    value: None
                }
            );
        }
        assert_eq!(
            run(b"VA 16777217\r\n", ResponseKind::Meta)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn server_errors_are_not_misses_for_any_response_kind() {
        for kind in [
            ResponseKind::Values { cas: false },
            ResponseKind::Stats,
            ResponseKind::Meta,
            ResponseKind::Simple,
            ResponseKind::Version,
        ] {
            assert_eq!(
                run(b"SERVER_ERROR out of memory\r\n", kind).unwrap(),
                WireResponse::Status("SERVER_ERROR out of memory".into())
            );
        }
        assert_eq!(
            run(b"ERROR\r\n", ResponseKind::Meta).unwrap(),
            WireResponse::Status("ERROR".into())
        );
        assert_eq!(
            run(b"CLIENT_ERROR bad command line\r\n", ResponseKind::Simple).unwrap(),
            WireResponse::Status("CLIENT_ERROR bad command line".into())
        );
    }

    #[test]
    fn cancelled_before_write_sends_nothing() {
        let (mut wire, written) = transport(b"END\r\n", 1);
        assert_eq!(
            wire.execute(
                b"delete key\r\n",
                ResponseKind::Simple,
                &AtomicBool::new(true)
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(written.borrow().is_empty());
    }
}
