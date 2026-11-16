use crate::config::Transport;
use native_tls::{Protocol, TlsConnector};
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const LINE_LIMIT: usize = 8192;
const LIST_LIMIT: usize = 1024;
const IO_LIMIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Resolve,
    Connect,
    Timeout,
    Disconnected,
    Protocol,
    Tls,
    Authentication,
    Stale,
    UnknownUps,
    Rejected,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Resolve => "Name resolution failed",
            Self::Connect => "Could not connect",
            Self::Timeout => "Server timed out",
            Self::Disconnected => "Connection closed",
            Self::Protocol => "Invalid NUT response",
            Self::Tls => "TLS negotiation or certificate validation failed",
            Self::Authentication => "NUT authentication failed",
            Self::Stale => "UPS data is stale",
            Self::UnknownUps => "UPS is unavailable",
            Self::Rejected => "The server rejected the NUT request",
        })
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        match err.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Self::Timeout,
            _ => Self::Disconnected,
        }
    }
}

pub type Values = BTreeMap<String, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    Line,
    Battery,
    Critical,
    Unknown,
}

pub fn has_fsd(values: &Values) -> bool {
    values
        .get("ups.status")
        .is_some_and(|flags| flags.split_ascii_whitespace().any(|flag| flag == "FSD"))
}

pub fn power(values: &Values) -> Power {
    let mut line = false;
    let mut battery = false;
    let mut low = false;
    for flag in values
        .get("ups.status")
        .into_iter()
        .flat_map(|s| s.split_ascii_whitespace())
    {
        match flag {
            "OL" => line = true,
            "OB" => battery = true,
            "LB" => low = true,
            "OFF" | "FSD" => return Power::Unknown,
            _ => {}
        }
    }
    match (line, battery, low) {
        (true, false, _) => Power::Line,
        (false, true, true) => Power::Critical,
        (false, true, false) => Power::Battery,
        _ => Power::Unknown,
    }
}

pub fn value<'a>(values: &'a Values, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|&key| {
        let text = values.get(key)?.trim();
        (!text.is_empty()
            && !text.eq_ignore_ascii_case("unknown")
            && !text.eq_ignore_ascii_case("n/a")
            && !matches!(text, "-" | "—"))
        .then_some(text)
    })
}

pub fn number(values: &Values, key: &str) -> Option<f64> {
    let raw = values.get(key)?.trim();
    let parsed = if matches!(
        key,
        "input.voltage"
            | "output.voltage"
            | "battery.voltage"
            | "input.frequency"
            | "output.frequency"
            | "ups.load"
            | "ups.realpower"
            | "output.realpower"
    ) && raw.contains(',')
        && !raw.contains('.')
    {
        raw.replace(',', ".").parse::<f64>()
    } else {
        raw.parse::<f64>()
    };
    parsed.ok().filter(|n| n.is_finite() && *n >= 0.0)
}

pub fn frequency_key(values: &Values) -> &'static str {
    if number(values, "input.frequency").is_none() && number(values, "output.frequency").is_some() {
        "output.frequency"
    } else {
        "input.frequency"
    }
}

pub fn real_power(values: &Values) -> Option<f64> {
    number(values, "ups.realpower").or_else(|| number(values, "output.realpower"))
}

fn tokens(line: &str) -> Result<Vec<String>, Error> {
    let mut result = Vec::new();
    let mut input = line.chars().peekable();
    while let Some(first) = input.peek().copied() {
        match first {
            ' ' | '\t' => {
                input.next();
                continue;
            }
            '#' => break,
            '=' => {
                input.next();
                result.push("=".into());
                continue;
            }
            _ => {}
        }
        let mut value = String::new();
        let quoted = first == '"';
        if quoted {
            input.next();
        }
        loop {
            match input.peek().copied() {
                Some(' ' | '\t' | '#' | '=') if !quoted => break,
                Some('"') if quoted => {
                    input.next();
                    break;
                }
                Some('\\') => {
                    input.next();
                    match input.next() {
                        Some(c) if !c.is_control() => value.push(c),
                        _ => return Err(Error::Protocol),
                    }
                }
                Some(c) if !c.is_control() => {
                    input.next();
                    value.push(c);
                }
                None if !quoted => break,
                _ => return Err(Error::Protocol),
            }
        }
        result.push(value);
    }
    Ok(result)
}

fn argument(input: &str) -> Result<String, Error> {
    if input.is_empty() || input.len() > 1024 || input.chars().any(char::is_control) {
        return Err(Error::Protocol);
    }
    let mut output = String::from("\"");
    for c in input.chars() {
        if matches!(c, '"' | '\\' | '#') {
            output.push('\\');
        }
        output.push(c);
    }
    output.push('"');
    Ok(output)
}

fn read_line(reader: &mut impl BufRead, bytes: &mut Vec<u8>) -> Result<Vec<String>, Error> {
    bytes.clear();
    let mut total = 0;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Err(Error::Disconnected);
        }
        let end = available.iter().position(|&byte| byte == b'\n');
        let count = end.map_or(available.len(), |index| index + 1);
        total += count;
        if total > LINE_LIMIT {
            return Err(Error::Protocol);
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if end.is_some() {
            let tail = if bytes.ends_with(b"\r\n") { 2 } else { 1 };
            let content = bytes.len() - tail;
            if bytes[..content]
                .iter()
                .rev()
                .take_while(|&&b| b == b'\\')
                .count()
                % 2
                == 1
            {
                bytes.truncate(content - 1);
                continue;
            }
            break;
        }
    }
    let line = String::from_utf8_lossy(bytes);
    let line = line.strip_suffix('\n').unwrap_or(&line);
    let parts = tokens(line.strip_suffix('\r').unwrap_or(line))?;
    if parts.first().is_some_and(|p| p == "ERR") {
        return Err(match parts.get(1).map(String::as_str) {
            Some("DATA-STALE" | "DRIVER-NOT-CONNECTED") => Error::Stale,
            Some("UNKNOWN-UPS") => Error::UnknownUps,
            _ => Error::Rejected,
        });
    }
    Ok(parts)
}

struct Bounded {
    socket: TcpStream,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    read_timeout: Duration,
    write_timeout: Duration,
}

impl Bounded {
    fn timeout(&self) -> io::Result<Duration> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        Ok(remaining.min(Duration::from_millis(500)))
    }
}

impl Read for Bounded {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            let timeout = self.timeout()?;
            if self.read_timeout != timeout {
                self.socket.set_read_timeout(Some(timeout))?;
                self.read_timeout = timeout;
            }
            match self.socket.read(bytes) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) => {}
                result => return result,
            }
        }
    }
}

impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let timeout = self.timeout()?;
        if self.write_timeout != timeout {
            self.socket.set_write_timeout(Some(timeout))?;
            self.write_timeout = timeout;
        }
        self.socket.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

enum Wire {
    Plain(Bounded),
    Secure(native_tls::TlsStream<Bounded>),
}

impl Wire {
    fn renew(&mut self) {
        let inner = match self {
            Self::Plain(socket) => socket,
            Self::Secure(socket) => socket.get_mut(),
        };
        inner.deadline = Instant::now() + IO_LIMIT;
    }
}

impl Read for Wire {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(socket) => socket.read(bytes),
            Self::Secure(socket) => socket.read(bytes),
        }
    }
}

impl Write for Wire {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(socket) => socket.write(bytes),
            Self::Secure(socket) => socket.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(socket) => socket.flush(),
            Self::Secure(socket) => socket.flush(),
        }
    }
}

pub struct Client {
    reader: BufReader<Wire>,
    line: Vec<u8>,
}

impl Client {
    pub fn new(
        socket: TcpStream,
        host: &str,
        transport: Transport,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, Error> {
        Self::connect(socket, host, transport, cancel, |_| {})
    }

    pub(crate) fn connect(
        socket: TcpStream,
        host: &str,
        transport: Transport,
        cancel: Arc<AtomicBool>,
        configure: impl FnOnce(&mut native_tls::TlsConnectorBuilder),
    ) -> Result<Self, Error> {
        let mut socket = Bounded {
            socket,
            deadline: Instant::now() + IO_LIMIT,
            cancel,
            read_timeout: Duration::ZERO,
            write_timeout: Duration::ZERO,
        };
        let mut line = Vec::new();
        if transport == Transport::Plain {
            return Ok(Self {
                reader: BufReader::new(Wire::Plain(socket)),
                line,
            });
        }
        if transport == Transport::StartTls {
            socket.write_all(b"STARTTLS\n").map_err(|_| Error::Tls)?;
            // Read exactly to the upgrade delimiter before handing the socket to TLS.
            if read_line(&mut BufReader::with_capacity(1, &mut socket), &mut line)
                .ok()
                .as_deref()
                != Some(&["OK".into(), "STARTTLS".into()])
            {
                return Err(Error::Tls);
            }
            socket.deadline = Instant::now() + IO_LIMIT;
        }
        let mut builder = TlsConnector::builder();
        builder.min_protocol_version(Some(Protocol::Tlsv12));
        configure(&mut builder);
        let connector = builder.build().map_err(|_| Error::Tls)?;
        let tls = connector.connect(host, socket).map_err(|_| Error::Tls)?;
        Ok(Self {
            reader: BufReader::new(Wire::Secure(tls)),
            line,
        })
    }

    fn send(&mut self, command: &str) -> Result<(), Error> {
        let wire = self.reader.get_mut();
        wire.renew();
        wire.write_all(command.as_bytes())?;
        wire.flush()?;
        Ok(())
    }

    pub fn login(&mut self, user: &str, password: &str) -> Result<(), Error> {
        if !matches!(self.reader.get_ref(), Wire::Secure(_)) {
            return Err(Error::Authentication);
        }
        let mut quoted = Zeroizing::new(argument(password).map_err(|_| Error::Authentication)?);
        let user = argument(user).map_err(|_| Error::Authentication)?;
        let username = format!("USERNAME {user}\n");
        let password = Zeroizing::new(format!("PASSWORD {}\n", quoted.as_str()));
        zeroize::Zeroize::zeroize(&mut *quoted);
        for command in [username.as_str(), password.as_str()] {
            self.send(command).map_err(|_| Error::Authentication)?;
            let ok = read_line(&mut self.reader, &mut self.line)
                .is_ok_and(|reply| reply.first().is_some_and(|p| p == "OK"));
            zeroize::Zeroize::zeroize(&mut self.line);
            if !ok {
                return Err(Error::Authentication);
            }
        }
        Ok(())
    }

    fn list(
        &mut self,
        what: &str,
        ups: Option<&str>,
        mut row: impl FnMut(Vec<String>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut header = vec!["LIST", what];
        let command = if let Some(name) = ups {
            header.push(name);
            format!("LIST {what} {}\n", argument(name)?)
        } else {
            format!("LIST {what}\n")
        };
        self.send(&command)?;
        let mut begin = vec!["BEGIN"];
        begin.extend_from_slice(&header);
        if read_line(&mut self.reader, &mut self.line)? != begin {
            return Err(Error::Protocol);
        }
        let mut end = vec!["END"];
        end.extend(header);
        for index in 0..=LIST_LIMIT {
            let parts = read_line(&mut self.reader, &mut self.line)?;
            if parts == end {
                return Ok(());
            }
            if index == LIST_LIMIT {
                return Err(Error::Protocol);
            }
            row(parts)?;
        }
        Err(Error::Protocol)
    }

    pub fn discover(&mut self) -> Result<Vec<String>, Error> {
        let mut names = Vec::new();
        self.list("UPS", None, |row| {
            let [kind, name, _] = &row[..] else {
                return Err(Error::Protocol);
            };
            if kind != "UPS" || name.is_empty() || name.len() > 255 {
                return Err(Error::Protocol);
            }
            names.push(name.clone());
            Ok(())
        })?;
        Ok(names)
    }

    pub fn poll(&mut self, ups: &str) -> Result<Values, Error> {
        let mut values = Values::new();
        self.list("VAR", Some(ups), |mut row| {
            if row.len() != 4 || row[0] != "VAR" || row[1] != ups || row[2].is_empty() {
                return Err(Error::Protocol);
            }
            let value = row.pop().ok_or(Error::Protocol)?;
            let key = row.pop().ok_or(Error::Protocol)?;
            if values.insert(key, value).is_some() {
                return Err(Error::Protocol);
            }
            Ok(())
        })?;
        Ok(values)
    }
}
