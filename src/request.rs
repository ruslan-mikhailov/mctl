use std::io::Write as _;

use crate::command::{BasicCommand, MetaCommand, StorageOperation};
use crate::wire::ResponseKind;

const MAX_HEADER: usize = 8 * 1024;

pub struct Request {
    pub bytes: Vec<u8>,
    pub response: ResponseKind,
}

impl Request {
    fn new(bytes: Vec<u8>, response: ResponseKind, header_len: usize) -> Result<Self, String> {
        if header_len > MAX_HEADER {
            return Err("request line exceeds 8 KiB limit".into());
        }
        Ok(Self { bytes, response })
    }
}

pub fn basic(command: &BasicCommand) -> Result<Request, String> {
    let mut bytes = Vec::new();
    let response = match command {
        BasicCommand::Get { keys, cas } => {
            write!(bytes, "{}", if *cas { "gets" } else { "get" }).unwrap();
            for key in keys {
                write!(bytes, " {key}").unwrap();
            }
            ResponseKind::Values { cas: *cas }
        }
        BasicCommand::Gat { ttl, keys, cas } => {
            write!(bytes, "{} {ttl}", if *cas { "gats" } else { "gat" }).unwrap();
            for key in keys {
                write!(bytes, " {key}").unwrap();
            }
            ResponseKind::Values { cas: *cas }
        }
        BasicCommand::Store {
            operation,
            key,
            value,
            ttl,
            flags,
            cas,
        } => {
            let (flags, ttl) = if matches!(
                operation,
                StorageOperation::Append | StorageOperation::Prepend
            ) {
                (0, 0)
            } else {
                (*flags, *ttl)
            };
            write!(
                bytes,
                "{} {key} {flags} {ttl} {}",
                operation.as_str(),
                value.len()
            )
            .unwrap();
            if let Some(cas) = cas {
                write!(bytes, " {cas}").unwrap();
            }
            let header_len = bytes.len() + 2;
            if header_len > MAX_HEADER {
                return Err("request line exceeds 8 KiB limit".into());
            }
            bytes.extend_from_slice(b"\r\n");
            bytes.extend_from_slice(value);
            bytes.extend_from_slice(b"\r\n");
            return Ok(Request {
                bytes,
                response: ResponseKind::Simple,
            });
        }
        BasicCommand::Delete { key } => {
            write!(bytes, "delete {key}").unwrap();
            ResponseKind::Simple
        }
        BasicCommand::Arithmetic {
            operation,
            key,
            delta,
        } => {
            write!(bytes, "{} {key} {delta}", operation.as_str()).unwrap();
            ResponseKind::Simple
        }
        BasicCommand::Touch { key, ttl } => {
            write!(bytes, "touch {key} {ttl}").unwrap();
            ResponseKind::Simple
        }
        BasicCommand::Stats(subcommand) => {
            bytes.extend_from_slice(b"stats");
            if let Some(subcommand) = subcommand {
                write!(bytes, " {}", subcommand.as_str()).unwrap();
            }
            ResponseKind::Stats
        }
        BasicCommand::Version => {
            bytes.extend_from_slice(b"version");
            ResponseKind::Version
        }
        BasicCommand::FlushAll { delay } => {
            bytes.extend_from_slice(b"flush_all");
            if *delay != 0 {
                write!(bytes, " {delay}").unwrap();
            }
            ResponseKind::Simple
        }
    };
    let header_len = bytes.len() + 2;
    bytes.extend_from_slice(b"\r\n");
    Request::new(bytes, response, header_len)
}

pub fn meta(command: &MetaCommand) -> Result<Request, String> {
    let mut bytes = Vec::new();
    let payload = match command {
        MetaCommand::Get { key, flags } => {
            write!(bytes, "mg {key}").unwrap();
            Some((flags.as_slice(), None))
        }
        MetaCommand::Set { key, value, flags } => {
            write!(bytes, "ms {key} {}", value.len()).unwrap();
            Some((flags.as_slice(), Some(value.as_slice())))
        }
        MetaCommand::Delete { key, flags } => {
            write!(bytes, "md {key}").unwrap();
            Some((flags.as_slice(), None))
        }
        MetaCommand::Arithmetic { key, flags } => {
            write!(bytes, "ma {key}").unwrap();
            Some((flags.as_slice(), None))
        }
        MetaCommand::Debug { key, binary } => {
            write!(bytes, "me {key}").unwrap();
            if *binary {
                bytes.extend_from_slice(b" b");
            }
            None
        }
        MetaCommand::Noop => {
            bytes.extend_from_slice(b"mn");
            None
        }
    };
    if let Some((flags, _)) = payload {
        for flag in flags {
            write!(bytes, " {}", flag.as_token()).unwrap();
        }
    }
    let header_len = bytes.len() + 2;
    if header_len > MAX_HEADER {
        return Err("request line exceeds 8 KiB limit".into());
    }
    bytes.extend_from_slice(b"\r\n");
    if let Some((_, Some(value))) = payload {
        bytes.extend_from_slice(value);
        bytes.extend_from_slice(b"\r\n");
    }
    Ok(Request {
        bytes,
        response: ResponseKind::Meta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, parse};

    fn encoded(input: &str) -> Request {
        match parse(input).unwrap() {
            Command::Basic(command) => basic(&command).unwrap(),
            Command::Meta(command) => meta(&command).unwrap(),
            Command::Local(_) => panic!("expected server command"),
        }
    }

    #[test]
    fn binary_storage_uses_exact_byte_length_and_terminator() {
        let request = encoded("set k --base64 AP8NCg== --ttl 30 --flags 4");
        assert_eq!(request.bytes, b"set k 4 30 4\r\n\0\xff\r\n\r\n");
        assert_eq!(request.response, ResponseKind::Simple);
        assert_eq!(encoded("append k ''").bytes, b"append k 0 0 0\r\n\r\n");
        assert_eq!(
            encoded("cas k x --cas 99").bytes,
            b"cas k 0 0 1 99\r\nx\r\n"
        );
    }

    #[test]
    fn basic_retrieval_and_diagnostics_encode_protocol_forms() {
        assert_eq!(encoded("gets a b").bytes, b"gets a b\r\n");
        assert_eq!(
            encoded("gats 30 a").response,
            ResponseKind::Values { cas: true }
        );
        assert_eq!(encoded("stats items").bytes, b"stats items\r\n");
        assert_eq!(encoded("touch a --ttl 5").bytes, b"touch a 5\r\n");
        assert_eq!(encoded("flush_all --delay 1").bytes, b"flush_all 1\r\n");
    }

    #[test]
    fn meta_framing_preserves_order_and_binary_keys() {
        assert_eq!(encoded("mg AP8= b t c v").bytes, b"mg AP8= b t c v\r\n");
        assert_eq!(
            encoded("ms a --base64 AP8= T60 F1").bytes,
            b"ms a 2 T60 F1\r\n\0\xff\r\n"
        );
        assert_eq!(encoded("md a I T30").bytes, b"md a I T30\r\n");
        assert_eq!(encoded("ma a D2 M+ v").bytes, b"ma a D2 M+ v\r\n");
        assert_eq!(encoded("me YQ== b").bytes, b"me YQ== b\r\n");
        assert_eq!(encoded("mn").bytes, b"mn\r\n");
        assert_eq!(
            encoded("inspect a --value").bytes,
            b"mg a f c t s h l v\r\n"
        );
    }
}
