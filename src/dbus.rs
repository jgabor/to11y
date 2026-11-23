//! Reads one secret from the Secret Service over the D-Bus session bus.
//!
//! This is the part of D-Bus that to11y needs and no more: EXTERNAL auth,
//! method calls, and a decoder for the replies. It never asks the service to
//! unlock anything, so it never shows a prompt. A locked item is reported as
//! locked. Calls carry NO_AUTO_START, so a service that isn't running is
//! reported as missing rather than started.

use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::time::Duration;

const SECRETS: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE: &str = "org.freedesktop.Secret.Service";
const ITEM: &str = "org.freedesktop.Secret.Item";
const TIMEOUT: Duration = Duration::from_secs(3);
const MAX_HEADER: usize = 64 * 1024;
const MAX_BODY: usize = 1024 * 1024;
const MAX_DEPTH: usize = 32;
const NO_AUTO_START: u8 = 0x2;

/// What a lookup found.
#[derive(Debug, PartialEq)]
pub enum Lookup {
    Found(Vec<u8>),
    Locked,
    Missing,
}

#[derive(Debug)]
pub enum Error {
    /// No session bus address, or none that to11y can use.
    NoBus,
    /// No Secret Service is running on the bus.
    NoService,
    Io(io::Error),
    /// The bus or the service sent something to11y can't read.
    Protocol(&'static str),
    /// The service answered with a D-Bus error. Only its name is kept,
    /// since the message could hold anything.
    Remote(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoBus => write!(f, "no D-Bus session bus"),
            Error::NoService => write!(f, "no Secret Service is running"),
            Error::Io(error) => write!(f, "D-Bus: {error}"),
            Error::Protocol(what) => write!(f, "D-Bus: {what}"),
            Error::Remote(name) => write!(f, "Secret Service error {name}"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Error::Io(error)
    }
}

/// Looks up the first item whose attributes include all of `attributes`, on
/// the bus at `address` (a D-Bus address list, as in
/// `DBUS_SESSION_BUS_ADDRESS`).
pub fn lookup(address: &str, attributes: &[(&str, &str)]) -> Result<Lookup, Error> {
    let mut bus = Bus::connect(address)?;
    let reply = bus.call(
        SERVICE_PATH,
        SERVICE,
        "OpenSession",
        "sv",
        &Body::new().string("plain").variant_string("").0,
    )?;
    let session = match reply.as_slice() {
        [_, Value::Path(session)] => session.clone(),
        _ => return Err(Error::Protocol("an unexpected OpenSession reply")),
    };
    let reply = bus.call(
        SERVICE_PATH,
        SERVICE,
        "SearchItems",
        "a{ss}",
        &Body::new().dict(attributes).0,
    )?;
    let (unlocked, locked) = match reply.as_slice() {
        [Value::Array(unlocked), Value::Array(locked)] => (unlocked, locked),
        _ => return Err(Error::Protocol("an unexpected SearchItems reply")),
    };
    let Some(Value::Path(item)) = unlocked.first() else {
        return Ok(if locked.is_empty() {
            Lookup::Missing
        } else {
            Lookup::Locked
        });
    };
    let reply = bus.call(
        item,
        ITEM,
        "GetSecret",
        "o",
        &Body::new().string(&session).0,
    )?;
    match reply.as_slice() {
        [Value::Struct(fields)] => match fields.as_slice() {
            [
                Value::Path(_),
                Value::Bytes(_),
                Value::Bytes(secret),
                Value::Str(_),
            ] => Ok(Lookup::Found(secret.clone())),
            _ => Err(Error::Protocol("an unexpected secret")),
        },
        _ => Err(Error::Protocol("an unexpected GetSecret reply")),
    }
}

/// The first usable socket in a D-Bus address list.
fn socket(address: &str) -> Result<UnixStream, Error> {
    let mut last = Error::NoBus;
    for entry in address.split(';') {
        let Some(params) = entry.strip_prefix("unix:") else {
            continue;
        };
        let mut path = None;
        let mut abstract_name = None;
        for param in params.split(',') {
            match param.split_once('=') {
                Some(("path", value)) => path = Some(unescape(value)?),
                Some(("abstract", value)) => abstract_name = Some(unescape(value)?),
                _ => {}
            }
        }
        let connected = if let Some(path) = path {
            UnixStream::connect(std::ffi::OsStr::from_bytes(&path))
        } else if let Some(name) = abstract_name {
            connect_abstract(&name)
        } else {
            continue;
        };
        match connected {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Error::Io(error),
        }
    }
    Err(last)
}

#[cfg(target_os = "linux")]
fn connect_abstract(name: &[u8]) -> io::Result<UnixStream> {
    use std::os::linux::net::SocketAddrExt;
    let address = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
    UnixStream::connect_addr(&address)
}

#[cfg(not(target_os = "linux"))]
fn connect_abstract(_: &[u8]) -> io::Result<UnixStream> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "abstract sockets are Linux only",
    ))
}

/// Undoes the `%XX` escapes of a D-Bus address value.
fn unescape(value: &str) -> Result<Vec<u8>, Error> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or(Error::NoBus)?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

struct Bus {
    stream: UnixStream,
    serial: u32,
}

impl Bus {
    fn connect(address: &str) -> Result<Bus, Error> {
        let mut stream = socket(address)?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        let uid = std::fs::metadata("/proc/self")?.uid();
        let hex: String = uid
            .to_string()
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect();
        stream.write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes())?;
        if !auth_line(&mut stream)?.starts_with("OK ") {
            return Err(Error::Protocol("the bus refused EXTERNAL auth"));
        }
        stream.write_all(b"BEGIN\r\n")?;
        let mut bus = Bus { stream, serial: 0 };
        bus.send(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "Hello",
            "",
            &[],
            0,
        )?;
        bus.reply()?;
        Ok(bus)
    }

    fn call(
        &mut self,
        path: &str,
        interface: &str,
        member: &str,
        signature: &str,
        body: &[u8],
    ) -> Result<Vec<Value>, Error> {
        self.send(
            Some(SECRETS),
            path,
            interface,
            member,
            signature,
            body,
            NO_AUTO_START,
        )?;
        self.reply()
    }

    #[allow(clippy::too_many_arguments)]
    fn send(
        &mut self,
        destination: Option<&str>,
        path: &str,
        interface: &str,
        member: &str,
        signature: &str,
        body: &[u8],
        flags: u8,
    ) -> Result<(), Error> {
        self.serial += 1;
        let message = encode_call(
            self.serial,
            destination,
            path,
            interface,
            member,
            signature,
            body,
            flags,
        );
        self.stream.write_all(&message)?;
        Ok(())
    }

    /// The reply to the last call, skipping signals and other messages.
    fn reply(&mut self) -> Result<Vec<Value>, Error> {
        loop {
            let message = read_message(&mut self.stream)?;
            if message.reply_serial != Some(self.serial) {
                continue;
            }
            match message.kind {
                METHOD_RETURN => {
                    return decode_body(&message.signature, &message.body, message.big);
                }
                ERROR => {
                    let name = message.error_name.unwrap_or_default();
                    return Err(match name.as_str() {
                        "org.freedesktop.DBus.Error.ServiceUnknown"
                        | "org.freedesktop.DBus.Error.NameHasNoOwner" => Error::NoService,
                        _ => Error::Remote(name),
                    });
                }
                _ => continue,
            }
        }
    }
}

/// One line of the auth exchange, without its `\r\n`.
fn auth_line(stream: &mut UnixStream) -> Result<String, Error> {
    let mut line = Vec::new();
    let mut byte = [0u8];
    while !line.ends_with(b"\r\n") {
        if line.len() > 512 {
            return Err(Error::Protocol("an auth line that is too long"));
        }
        stream.read_exact(&mut byte)?;
        line.push(byte[0]);
    }
    line.truncate(line.len() - 2);
    String::from_utf8(line).map_err(|_| Error::Protocol("an auth line that isn't UTF-8"))
}

const METHOD_CALL: u8 = 1;
const METHOD_RETURN: u8 = 2;
const ERROR: u8 = 3;

/// A message body or header under construction, little-endian. Offsets are
/// relative to its start, which sits on an 8-byte boundary of the message.
struct Body(Vec<u8>);

impl Body {
    fn new() -> Self {
        Body(Vec::new())
    }

    fn pad(&mut self, align: usize) {
        while !self.0.len().is_multiple_of(align) {
            self.0.push(0);
        }
    }

    fn byte(mut self, byte: u8) -> Self {
        self.0.push(byte);
        self
    }

    fn u32(mut self, value: u32) -> Self {
        self.pad(4);
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// A string or an object path.
    fn string(self, value: &str) -> Self {
        let mut this = self.u32(value.len() as u32);
        this.0.extend_from_slice(value.as_bytes());
        this.0.push(0);
        this
    }

    fn signature(mut self, value: &str) -> Self {
        self.0.push(value.len() as u8);
        self.0.extend_from_slice(value.as_bytes());
        self.0.push(0);
        self
    }

    fn variant_string(self, value: &str) -> Self {
        self.signature("s").string(value)
    }

    /// An array whose elements start on `align`, filled by `fill`.
    fn array(self, align: usize, fill: impl FnOnce(Self) -> Self) -> Self {
        let mut this = self.u32(0);
        let length_at = this.0.len() - 4;
        this.pad(align);
        let start = this.0.len();
        let mut this = fill(this);
        let length = (this.0.len() - start) as u32;
        this.0[length_at..length_at + 4].copy_from_slice(&length.to_le_bytes());
        this
    }

    fn dict(self, pairs: &[(&str, &str)]) -> Self {
        self.array(8, |mut body| {
            for (key, value) in pairs {
                body.pad(8);
                body = body.string(key).string(value);
            }
            body
        })
    }

    /// A header field: a `(yv)` struct.
    fn field(mut self, code: u8, signature: &str, write: impl FnOnce(Self) -> Self) -> Self {
        self.pad(8);
        write(self.byte(code).signature(signature))
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_call(
    serial: u32,
    destination: Option<&str>,
    path: &str,
    interface: &str,
    member: &str,
    signature: &str,
    body: &[u8],
    flags: u8,
) -> Vec<u8> {
    let header = Body::new()
        .byte(b'l')
        .byte(METHOD_CALL)
        .byte(flags)
        .byte(1)
        .u32(body.len() as u32)
        .u32(serial)
        .array(8, |fields| {
            let mut fields = fields
                .field(1, "o", |b| b.string(path))
                .field(2, "s", |b| b.string(interface))
                .field(3, "s", |b| b.string(member));
            if let Some(destination) = destination {
                fields = fields.field(6, "s", |b| b.string(destination));
            }
            if !signature.is_empty() {
                fields = fields.field(8, "g", |b| b.signature(signature));
            }
            fields
        });
    let mut message = header;
    message.pad(8);
    message.0.extend_from_slice(body);
    message.0
}

struct Message {
    kind: u8,
    big: bool,
    reply_serial: Option<u32>,
    error_name: Option<String>,
    signature: String,
    body: Vec<u8>,
}

fn read_message(stream: &mut impl Read) -> Result<Message, Error> {
    let mut fixed = [0u8; 16];
    stream.read_exact(&mut fixed)?;
    let big = match fixed[0] {
        b'l' => false,
        b'B' => true,
        _ => return Err(Error::Protocol("a message with no byte order")),
    };
    let word = |at: usize| {
        let bytes = [fixed[at], fixed[at + 1], fixed[at + 2], fixed[at + 3]];
        if big {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        }
    };
    let body_length = word(4) as usize;
    let fields_length = word(12) as usize;
    if fields_length > MAX_HEADER || body_length > MAX_BODY {
        return Err(Error::Protocol("a message that is too large"));
    }
    let header_length = (16 + fields_length).next_multiple_of(8);
    let mut header = fixed.to_vec();
    header.resize(header_length, 0);
    stream.read_exact(&mut header[16..])?;
    let mut body = vec![0u8; body_length];
    stream.read_exact(&mut body)?;

    let mut reader = Reader {
        buffer: &header,
        position: 12,
        big,
        depth: 0,
    };
    let fields = reader.value(b"a(yv)", &mut 0)?;
    let mut message = Message {
        kind: fixed[1],
        big,
        reply_serial: None,
        error_name: None,
        signature: String::new(),
        body,
    };
    let Value::Array(fields) = fields else {
        return Err(Error::Protocol("a malformed header"));
    };
    for field in fields {
        let Value::Struct(pair) = field else { continue };
        let [Value::Byte(code), Value::Variant(value)] = pair.as_slice() else {
            continue;
        };
        match (code, value.as_ref()) {
            (4, Value::Str(name)) => message.error_name = Some(name.clone()),
            (5, Value::U32(serial)) => message.reply_serial = Some(*serial),
            (8, Value::Signature(signature)) => message.signature = signature.clone(),
            _ => {}
        }
    }
    Ok(message)
}

fn decode_body(signature: &str, body: &[u8], big: bool) -> Result<Vec<Value>, Error> {
    let mut reader = Reader {
        buffer: body,
        position: 0,
        big,
        depth: 0,
    };
    let signature = signature.as_bytes();
    let mut values = Vec::new();
    let mut at = 0;
    while at < signature.len() {
        values.push(reader.value(signature, &mut at)?);
    }
    Ok(values)
}

/// A decoded D-Bus value. `ay` decodes to `Bytes`, and a dict to an array
/// of two-field structs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Byte(u8),
    Bool(bool),
    U32(u32),
    Int(i64),
    Uint(u64),
    Double(f64),
    Str(String),
    Path(String),
    Signature(String),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    Struct(Vec<Value>),
    Variant(Box<Value>),
}

struct Reader<'a> {
    buffer: &'a [u8],
    position: usize,
    big: bool,
    depth: usize,
}

impl Reader<'_> {
    fn align(&mut self, align: usize) -> Result<(), Error> {
        let to = self.position.next_multiple_of(align);
        if to > self.buffer.len() {
            return Err(Error::Protocol("a truncated value"));
        }
        self.position = to;
        Ok(())
    }

    fn take(&mut self, length: usize) -> Result<&[u8], Error> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.buffer.len())
            .ok_or(Error::Protocol("a truncated value"))?;
        let bytes = &self.buffer[self.position..end];
        self.position = end;
        Ok(bytes)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        self.align(N)?;
        let mut bytes: [u8; N] = self.take(N)?.try_into().expect("N bytes");
        if self.big {
            bytes.reverse();
        }
        Ok(bytes)
    }

    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.fixed::<4>()?))
    }

    fn text(&mut self, length: usize) -> Result<String, Error> {
        let bytes = self.take(length + 1)?;
        if bytes[length] != 0 {
            return Err(Error::Protocol("a string with no terminator"));
        }
        String::from_utf8(bytes[..length].to_vec())
            .map_err(|_| Error::Protocol("a string that isn't UTF-8"))
    }

    /// Decodes the complete type that starts at `signature[*at]`, and moves
    /// `at` past it.
    fn value(&mut self, signature: &[u8], at: &mut usize) -> Result<Value, Error> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Error::Protocol("a value nested too deeply"));
        }
        let code = *signature
            .get(*at)
            .ok_or(Error::Protocol("a truncated signature"))?;
        *at += 1;
        let value = match code {
            b'y' => Value::Byte(self.take(1)?[0]),
            b'b' => Value::Bool(self.u32()? != 0),
            b'n' => Value::Int(i16::from_le_bytes(self.fixed::<2>()?).into()),
            b'q' => Value::Uint(u16::from_le_bytes(self.fixed::<2>()?).into()),
            b'i' => Value::Int(i32::from_le_bytes(self.fixed::<4>()?).into()),
            b'u' | b'h' => Value::U32(self.u32()?),
            b'x' => Value::Int(i64::from_le_bytes(self.fixed::<8>()?)),
            b't' => Value::Uint(u64::from_le_bytes(self.fixed::<8>()?)),
            b'd' => Value::Double(f64::from_le_bytes(self.fixed::<8>()?)),
            b's' => {
                let length = self.u32()? as usize;
                Value::Str(self.text(length)?)
            }
            b'o' => {
                let length = self.u32()? as usize;
                Value::Path(self.text(length)?)
            }
            b'g' => {
                let length = self.take(1)?[0] as usize;
                Value::Signature(self.text(length)?)
            }
            b'v' => {
                let length = self.take(1)?[0] as usize;
                let inner = self.text(length)?;
                let mut inner_at = 0;
                let value = self.value(inner.as_bytes(), &mut inner_at)?;
                if inner_at != inner.len() {
                    return Err(Error::Protocol("a variant with more than one type"));
                }
                Value::Variant(Box::new(value))
            }
            b'a' => {
                let length = self.u32()? as usize;
                let element = *at;
                let element_end = skip_type(signature, element)?;
                self.align(alignment(signature[element]))?;
                let end = self
                    .position
                    .checked_add(length)
                    .filter(|end| *end <= self.buffer.len())
                    .ok_or(Error::Protocol("a truncated array"))?;
                let value = if signature[element] == b'y' {
                    Value::Bytes(self.take(length)?.to_vec())
                } else {
                    let mut items = Vec::new();
                    while self.position < end {
                        let mut item_at = element;
                        items.push(self.value(signature, &mut item_at)?);
                    }
                    if self.position != end {
                        return Err(Error::Protocol("an array longer than its length"));
                    }
                    Value::Array(items)
                };
                *at = element_end;
                value
            }
            b'(' | b'{' => {
                let close = if code == b'(' { b')' } else { b'}' };
                self.align(8)?;
                let mut fields = Vec::new();
                while signature.get(*at) != Some(&close) {
                    if *at >= signature.len() {
                        return Err(Error::Protocol("an unclosed struct"));
                    }
                    fields.push(self.value(signature, at)?);
                }
                *at += 1;
                Value::Struct(fields)
            }
            _ => return Err(Error::Protocol("an unknown type")),
        };
        self.depth -= 1;
        Ok(value)
    }
}

fn alignment(code: u8) -> usize {
    match code {
        b'n' | b'q' => 2,
        b'b' | b'i' | b'u' | b'h' | b's' | b'o' | b'a' => 4,
        b'x' | b't' | b'd' | b'(' | b'{' => 8,
        _ => 1,
    }
}

/// The index just past the complete type that starts at `signature[at]`.
fn skip_type(signature: &[u8], at: usize) -> Result<usize, Error> {
    let truncated = Error::Protocol("a truncated signature");
    match *signature.get(at).ok_or(truncated)? {
        b'a' => skip_type(signature, at + 1),
        open @ (b'(' | b'{') => {
            let close = if open == b'(' { b')' } else { b'}' };
            let mut at = at + 1;
            while *signature
                .get(at)
                .ok_or(Error::Protocol("an unclosed struct"))?
                != close
            {
                at = skip_type(signature, at)?;
            }
            Ok(at + 1)
        }
        _ => Ok(at + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_value_is_unescaped() {
        assert_eq!(
            unescape("/run/user/1000/bus").unwrap(),
            b"/run/user/1000/bus"
        );
        assert_eq!(unescape("/tmp/a%20b%2c").unwrap(), b"/tmp/a b,");
        assert!(unescape("/tmp/%zz").is_err());
    }

    #[test]
    fn an_address_with_no_unix_entry_has_no_bus() {
        assert!(matches!(
            socket("tcp:host=localhost,port=1"),
            Err(Error::NoBus)
        ));
    }

    #[test]
    fn a_call_decodes_back_to_its_fields() {
        let body = Body::new().dict(&[("service", "gemini"), ("username", "antigravity")]);
        let message = encode_call(
            7,
            Some(SECRETS),
            SERVICE_PATH,
            SERVICE,
            "SearchItems",
            "a{ss}",
            &body.0,
            NO_AUTO_START,
        );
        let mut stream = message.as_slice();
        let decoded = read_message(&mut stream).unwrap();
        assert_eq!(decoded.kind, METHOD_CALL);
        assert_eq!(decoded.signature, "a{ss}");
        let values = decode_body(&decoded.signature, &decoded.body, false).unwrap();
        assert_eq!(
            values,
            vec![Value::Array(vec![
                Value::Struct(vec![
                    Value::Str("service".into()),
                    Value::Str("gemini".into())
                ]),
                Value::Struct(vec![
                    Value::Str("username".into()),
                    Value::Str("antigravity".into())
                ]),
            ])]
        );
    }

    #[test]
    fn a_big_endian_reply_decodes() {
        // A method return with REPLY_SERIAL 1 and SIGNATURE "u", body 5.
        let mut message = vec![
            b'B',
            METHOD_RETURN,
            0,
            1,
            0,
            0,
            0,
            4,
            0,
            0,
            0,
            9,
            0,
            0,
            0,
            0,
        ];
        let fields: &[u8] = &[
            5, 1, b'u', 0, 0, 0, 0, 1, // (5, <u 1>)
            8, 1, b'g', 0, 1, b'u', 0, // (8, <g "u">)
        ];
        message[12..16].copy_from_slice(&(fields.len() as u32).to_be_bytes());
        message.extend_from_slice(fields);
        while message.len() % 8 != 0 {
            message.push(0);
        }
        message.extend_from_slice(&5u32.to_be_bytes());
        let decoded = read_message(&mut message.as_slice()).unwrap();
        assert_eq!(decoded.reply_serial, Some(1));
        let values = decode_body(&decoded.signature, &decoded.body, decoded.big).unwrap();
        assert_eq!(values, vec![Value::U32(5)]);
    }

    #[test]
    fn random_bytes_never_panic_the_decoder() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let signatures: [&[u8]; 6] = [b"a{ss}", b"(oayays)", b"aoao", b"v", b"a(yv)", b"sv"];
        for round in 0..20_000 {
            let length = (next() % 96) as usize;
            let bytes: Vec<u8> = (0..length).map(|_| next() as u8).collect();
            let signature = signatures[round % signatures.len()];
            let mut reader = Reader {
                buffer: &bytes,
                position: 0,
                big: round % 2 == 0,
                depth: 0,
            };
            let _ = reader.value(signature, &mut 0);
            let _ = read_message(&mut bytes.as_slice());
        }
    }

    /// A Secret Service written with zbus, on a private `dbus-daemon`.
    /// zbus is the oracle: its encoder, not ours, frames every reply.
    mod against_zbus {
        use super::super::*;
        use std::collections::HashMap;
        use std::io::BufRead;
        use std::process::{Child, Command, Stdio};
        use zbus::{OwnedObjectPath, OwnedVariant, Variant};

        const LOGIN: &str = "/org/freedesktop/secrets/collection/login";

        struct Service {
            /// Attributes, item path, and whether its collection is locked.
            items: Vec<(HashMap<String, String>, String, bool)>,
        }

        #[zbus::interface(name = "org.freedesktop.Secret.Service")]
        impl Service {
            #[zbus(out_args("output", "result"))]
            fn open_session(
                &self,
                algorithm: &str,
                _input: Variant<'_>,
            ) -> zbus::fdo::Result<(OwnedVariant, OwnedObjectPath)> {
                if algorithm != "plain" {
                    return Err(zbus::fdo::Error::NotSupported(algorithm.to_owned()));
                }
                let output = OwnedVariant::try_from(Variant::new("")).unwrap();
                let path =
                    OwnedObjectPath::try_from("/org/freedesktop/secrets/session/s1").unwrap();
                Ok((output, path))
            }

            #[zbus(out_args("unlocked", "locked"))]
            fn search_items(
                &self,
                attributes: HashMap<String, String>,
            ) -> (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) {
                let (mut unlocked, mut locked) = (Vec::new(), Vec::new());
                for (item, path, is_locked) in &self.items {
                    if attributes.iter().all(|(k, v)| item.get(k) == Some(v)) {
                        let path = OwnedObjectPath::try_from(path.as_str()).unwrap();
                        if *is_locked {
                            locked.push(path)
                        } else {
                            unlocked.push(path)
                        }
                    }
                }
                (unlocked, locked)
            }
        }

        struct Item {
            secret: Vec<u8>,
        }

        #[zbus::interface(name = "org.freedesktop.Secret.Item")]
        impl Item {
            #[zbus(out_args("secret"))]
            fn get_secret(
                &self,
                session: OwnedObjectPath,
            ) -> ((OwnedObjectPath, Vec<u8>, Vec<u8>, String),) {
                ((
                    session,
                    Vec::new(),
                    self.secret.clone(),
                    "text/plain".to_owned(),
                ),)
            }
        }

        /// A session bus of our own, with no service activation.
        struct Daemon {
            child: Child,
            address: String,
            _dir: crate::testing::TempDir,
        }

        impl Daemon {
            fn start() -> Self {
                let dir = crate::testing::TempDir::new("dbus");
                let config = dir.write(
                    "bus.conf",
                    &format!(
                        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}/bus</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>"#,
                        dir.path().display()
                    ),
                );
                let mut child = Command::new("dbus-daemon")
                    .arg(format!("--config-file={}", config.display()))
                    .args(["--nofork", "--print-address=1"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("dbus-daemon is installed");
                let mut address = String::new();
                std::io::BufReader::new(child.stdout.take().unwrap())
                    .read_line(&mut address)
                    .unwrap();
                Daemon {
                    child,
                    address: address.trim().to_owned(),
                    _dir: dir,
                }
            }
        }

        impl Drop for Daemon {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }

        fn attributes(pairs: &[(&str, &str)]) -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        }

        #[test]
        fn lookups_agree_with_a_zbus_secret_service() {
            let daemon = Daemon::start();
            let address = daemon.address.clone();
            let lookup_now = |pairs: &'static [(&'static str, &'static str)]| {
                let address = address.clone();
                std::thread::spawn(move || lookup(&address, pairs))
                    .join()
                    .unwrap()
            };
            // Nobody owns the name yet, and the bus can't start anyone.
            assert!(matches!(
                lookup_now(&[("service", "gemini")]),
                Err(Error::NoService)
            ));

            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let secret: Vec<u8> = (0..=255).collect();
            let _connection = runtime.block_on(async {
                let service = Service {
                    items: vec![
                        (
                            attributes(&[("service", "gemini"), ("username", "antigravity")]),
                            format!("{LOGIN}/1"),
                            false,
                        ),
                        (
                            attributes(&[
                                ("service", "Factory CLI"),
                                ("account", "auth-encryption-key"),
                            ]),
                            format!("{LOGIN}/2"),
                            true,
                        ),
                    ],
                };
                zbus::connection::Builder::address(daemon.address.as_str())
                    .name("org.freedesktop.secrets")
                    .serve_at("/org/freedesktop/secrets", service)
                    .serve_at(
                        format!("{LOGIN}/1"),
                        Item {
                            secret: secret.clone(),
                        },
                    )
                    .serve_at(
                        format!("{LOGIN}/2"),
                        Item {
                            secret: b"locked".to_vec(),
                        },
                    )
                    .build()
                    .await
                    .unwrap()
            });

            let found = lookup_now(&[("service", "gemini"), ("username", "antigravity")]).unwrap();
            assert_eq!(found, Lookup::Found(secret));
            let locked = lookup_now(&[
                ("service", "Factory CLI"),
                ("account", "auth-encryption-key"),
            ]);
            assert_eq!(locked.unwrap(), Lookup::Locked);
            assert_eq!(lookup_now(&[("service", "none")]).unwrap(), Lookup::Missing);
        }
    }
}
