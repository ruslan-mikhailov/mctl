//! Typed, locally validated REPL commands. No input is ever passed through to the socket.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Read;

use base64::Engine as _;

pub const MAX_VALUE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_RELATIVE_TTL: u32 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Basic(BasicCommand),
    Meta(MetaCommand),
    Local(LocalCommand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageOperation {
    Set,
    Add,
    Replace,
    Append,
    Prepend,
    Cas,
}

impl StorageOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Add => "add",
            Self::Replace => "replace",
            Self::Append => "append",
            Self::Prepend => "prepend",
            Self::Cas => "cas",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithmeticOperation {
    Incr,
    Decr,
}

impl ArithmeticOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Incr => "incr",
            Self::Decr => "decr",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsSubcommand {
    Items,
    Slabs,
    Settings,
    Sizes,
}

impl StatsSubcommand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Items => "items",
            Self::Slabs => "slabs",
            Self::Settings => "settings",
            Self::Sizes => "sizes",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BasicCommand {
    Get {
        keys: Vec<String>,
        cas: bool,
    },
    Gat {
        ttl: u32,
        keys: Vec<String>,
        cas: bool,
    },
    Store {
        operation: StorageOperation,
        key: String,
        value: Vec<u8>,
        ttl: u32,
        flags: u32,
        cas: Option<u64>,
    },
    Delete {
        key: String,
    },
    Arithmetic {
        operation: ArithmeticOperation,
        key: String,
        delta: u64,
    },
    Touch {
        key: String,
        ttl: u32,
    },
    Stats(Option<StatsSubcommand>),
    Version,
    FlushAll {
        delay: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaCommand {
    Get {
        key: String,
        flags: Vec<MetaFlag>,
    },
    Set {
        key: String,
        value: Vec<u8>,
        flags: Vec<MetaFlag>,
    },
    Delete {
        key: String,
        flags: Vec<MetaFlag>,
    },
    Arithmetic {
        key: String,
        flags: Vec<MetaFlag>,
    },
    Debug {
        key: String,
        binary: bool,
    },
    Noop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaFlag {
    BinaryKey,
    Cas,
    CompareCas(u64),
    ClientFlags,
    Hit,
    Key,
    LastAccess,
    Opaque(String),
    Size,
    RemainingTtl,
    NoLruBump,
    Value,
    OverrideCas(u64),
    Vivify(u32),
    Recache(u32),
    Ttl(u32),
    SetClientFlags(u32),
    Invalidate,
    Mode(char),
    Initial(u64),
    Delta(u64),
    RemoveValue,
}

impl MetaFlag {
    pub fn as_token(&self) -> String {
        match self {
            Self::BinaryKey => "b".into(),
            Self::Cas => "c".into(),
            Self::CompareCas(n) => format!("C{n}"),
            Self::ClientFlags => "f".into(),
            Self::Hit => "h".into(),
            Self::Key => "k".into(),
            Self::LastAccess => "l".into(),
            Self::Opaque(s) => format!("O{s}"),
            Self::Size => "s".into(),
            Self::RemainingTtl => "t".into(),
            Self::NoLruBump => "u".into(),
            Self::Value => "v".into(),
            Self::OverrideCas(n) => format!("E{n}"),
            Self::Vivify(n) => format!("N{n}"),
            Self::Recache(n) => format!("R{n}"),
            Self::Ttl(n) => format!("T{n}"),
            Self::SetClientFlags(n) => format!("F{n}"),
            Self::Invalidate => "I".into(),
            Self::Mode(c) => format!("M{c}"),
            Self::Initial(n) => format!("J{n}"),
            Self::Delta(n) => format!("D{n}"),
            Self::RemoveValue => "x".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalCommand {
    Help(Option<String>),
    History,
    Recent(RecentCommand),
    Reconnect,
    Quit,
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecentCommand {
    List,
    Forget { address: String, tls: Option<bool> },
    Clear,
}

impl Command {
    /// Only commands explicitly listed here may run in a read-only session.
    pub fn is_readonly(&self) -> bool {
        match self {
            Self::Basic(
                BasicCommand::Get { .. } | BasicCommand::Stats(_) | BasicCommand::Version,
            ) => true,
            Self::Meta(MetaCommand::Debug { .. } | MetaCommand::Noop) => true,
            Self::Local(LocalCommand::Recent(_)) => true,
            Self::Local(
                LocalCommand::Help(_)
                | LocalCommand::History
                | LocalCommand::Reconnect
                | LocalCommand::Quit
                | LocalCommand::Exit,
            ) => true,
            _ => false,
        }
    }

    pub fn readonly_error(&self) -> Option<&'static str> {
        if self.is_readonly() {
            return None;
        }
        Some(match self {
            Self::Basic(BasicCommand::Gat { .. } | BasicCommand::Touch { .. }) => {
                READONLY_EXPIRATION
            }
            Self::Basic(BasicCommand::FlushAll { .. }) => READONLY_FLUSH,
            Self::Meta(MetaCommand::Get { .. }) => READONLY_RECACHE,
            _ => READONLY_MUTATION,
        })
    }
}

const READONLY_EXPIRATION: &str = "readonly: command changes expiration; no request sent";
const READONLY_FLUSH: &str = "readonly: flush_all invalidates cached items; no request sent";
const READONLY_RECACHE: &str =
    "readonly: mg/inspect may claim stale-item recache ownership; no request sent";
const READONLY_MUTATION: &str = "readonly: command changes cached items; no request sent";

fn readonly_error_for_words(words: &[String]) -> Option<&'static str> {
    let verb = words.first()?.as_str();
    match verb {
        "gat" | "gats" | "touch" => Some(READONLY_EXPIRATION),
        "flush_all" => Some(READONLY_FLUSH),
        "mg" | "inspect" => Some(READONLY_RECACHE),
        "set" | "add" | "replace" | "append" | "prepend" | "cas" | "delete" | "incr" | "decr"
        | "ms" | "md" | "ma" => Some(READONLY_MUTATION),
        _ => None,
    }
}

pub fn parse(line: &str) -> Result<Command, String> {
    parse_for_session(line, false)
}

/// Reject universally forbidden verbs before parsing values or options, so an
/// incomplete mutation in a read-only session reports the mode restriction.
pub fn parse_for_session(line: &str, readonly: bool) -> Result<Command, String> {
    let words = shell_words::split(line).map_err(|error| format!("invalid quoting: {error}"))?;
    if readonly {
        if let Some(error) = readonly_error_for_words(&words) {
            return Err(error.into());
        }
    }
    let Some((verb, args)) = words.split_first() else {
        return Err("enter a command (try help)".into());
    };
    let basic = |command| Ok(Command::Basic(command));
    let meta = |command| Ok(Command::Meta(command));
    let local = |command| Ok(Command::Local(command));
    match verb.as_str() {
        "get" | "gets" => {
            if args.is_empty() {
                return Err(format!("usage: {verb} KEY [KEY...]"));
            }
            for key in args {
                validate_key(key)?;
            }
            validate_multi_key_line(verb.len(), args)?;
            basic(BasicCommand::Get {
                keys: args.to_vec(),
                cas: verb == "gets",
            })
        }
        "gat" | "gats" => {
            let (ttl, keys) = args
                .split_first()
                .ok_or_else(|| format!("usage: {verb} SEC KEY [KEY...]"))?;
            if keys.is_empty() {
                return Err(format!("usage: {verb} SEC KEY [KEY...]"));
            }
            let ttl = relative_ttl(ttl)?;
            for key in keys {
                validate_key(key)?;
            }
            validate_multi_key_line(verb.len() + 1 + ttl.to_string().len(), keys)?;
            basic(BasicCommand::Gat {
                ttl,
                keys: keys.to_vec(),
                cas: verb == "gats",
            })
        }
        "set" | "add" | "replace" | "append" | "prepend" | "cas" => {
            let operation = match verb.as_str() {
                "set" => StorageOperation::Set,
                "add" => StorageOperation::Add,
                "replace" => StorageOperation::Replace,
                "append" => StorageOperation::Append,
                "prepend" => StorageOperation::Prepend,
                _ => StorageOperation::Cas,
            };
            let (key, args) = args
                .split_first()
                .ok_or_else(|| format!("usage: {verb} KEY VALUE [options]"))?;
            validate_key(key)?;
            let (value, args) = parse_value(args)?;
            let mut ttl = 0;
            let mut flags = 0;
            let mut cas = None;
            if args.first().is_some_and(|arg| arg.starts_with("--")) {
                let mut seen = HashSet::new();
                let mut rest = args;
                while let Some((option, tail)) = rest.split_first() {
                    if !option.starts_with("--") {
                        return Err("cannot mix positional numbers with named options".into());
                    }
                    let name = option
                        .split_once('=')
                        .map_or(option.as_str(), |(name, _)| name);
                    if !seen.insert(name) {
                        return Err(format!("duplicate option {name}"));
                    }
                    let (number, next) = option_value(option, tail, "requires a number")?;
                    match name {
                        "--ttl"
                            if !matches!(
                                operation,
                                StorageOperation::Append | StorageOperation::Prepend
                            ) =>
                        {
                            ttl = relative_ttl(number)?
                        }
                        "--flags"
                            if !matches!(
                                operation,
                                StorageOperation::Append | StorageOperation::Prepend
                            ) =>
                        {
                            flags = decimal::<u32>(number, "flags")?
                        }
                        "--cas" if operation == StorageOperation::Cas => {
                            cas = Some(decimal::<u64>(number, "CAS ID")?)
                        }
                        _ => return Err(format!("unsupported option {name} for {verb}")),
                    }
                    rest = next;
                }
            } else if !args.is_empty() {
                if args.iter().any(|arg| arg.starts_with("--")) {
                    return Err("cannot mix positional numbers with named options".into());
                }
                let max = match operation {
                    StorageOperation::Cas => 3,
                    StorageOperation::Append | StorageOperation::Prepend => 0,
                    _ => 2,
                };
                if args.len() > max {
                    return Err(format!("too many positional numbers for {verb}"));
                }
                let mut numbers = args.iter();
                if operation == StorageOperation::Cas {
                    cas = Some(decimal::<u64>(numbers.next().unwrap(), "CAS ID")?);
                }
                if let Some(number) = numbers.next() {
                    ttl = relative_ttl(number)?;
                }
                if let Some(number) = numbers.next() {
                    flags = decimal::<u32>(number, "flags")?;
                }
            }
            if operation == StorageOperation::Cas && cas.is_none() {
                return Err("cas requires CAS_ID or --cas ID".into());
            }
            basic(BasicCommand::Store {
                operation,
                key: key.clone(),
                value,
                ttl,
                flags,
                cas,
            })
        }
        "delete" => {
            exactly(args, 1, "delete KEY")?;
            validate_key(&args[0])?;
            basic(BasicCommand::Delete {
                key: args[0].clone(),
            })
        }
        "incr" | "decr" => {
            exactly(args, 2, &format!("{verb} KEY DELTA"))?;
            validate_key(&args[0])?;
            let delta = decimal::<u64>(&args[1], "delta")?;
            basic(BasicCommand::Arithmetic {
                operation: if verb == "incr" {
                    ArithmeticOperation::Incr
                } else {
                    ArithmeticOperation::Decr
                },
                key: args[0].clone(),
                delta,
            })
        }
        "touch" => {
            let ttl = match args {
                [key, ttl] if !ttl.starts_with("--") => {
                    validate_key(key)?;
                    relative_ttl(ttl)?
                }
                [key, option, ttl] if option == "--ttl" => {
                    validate_key(key)?;
                    relative_ttl(ttl)?
                }
                [key, option] if option.starts_with("--ttl=") => {
                    validate_key(key)?;
                    relative_ttl(&option["--ttl=".len()..])?
                }
                _ => return Err("usage: touch KEY TTL | touch KEY --ttl SEC".into()),
            };
            basic(BasicCommand::Touch {
                key: args[0].clone(),
                ttl,
            })
        }
        "stats" => {
            if args.len() > 1 {
                return Err("usage: stats [items|slabs|settings|sizes]".into());
            }
            let subcommand = match args.first().map(String::as_str) {
                None => None,
                Some("items") => Some(StatsSubcommand::Items),
                Some("slabs") => Some(StatsSubcommand::Slabs),
                Some("settings") => Some(StatsSubcommand::Settings),
                Some("sizes") => Some(StatsSubcommand::Sizes),
                Some(other) => return Err(format!("unsupported stats subcommand {other}")),
            };
            basic(BasicCommand::Stats(subcommand))
        }
        "version" => {
            exactly(args, 0, "version")?;
            basic(BasicCommand::Version)
        }
        "flush_all" => {
            let delay = match args {
                [] => 0,
                [delay] if !delay.starts_with("--") => relative_ttl(delay)?,
                [option, delay] if option == "--delay" => relative_ttl(delay)?,
                [option] if option.starts_with("--delay=") => {
                    relative_ttl(&option["--delay=".len()..])?
                }
                _ => return Err("usage: flush_all [DELAY | --delay SEC]".into()),
            };
            basic(BasicCommand::FlushAll { delay })
        }
        "mg" | "md" | "ma" | "me" => {
            let (key, tail) = args
                .split_first()
                .ok_or_else(|| format!("usage: {verb} KEY [FLAGS...]"))?;
            if verb == "me" {
                if !(tail.is_empty() || (tail.len() == 1 && tail[0] == "b")) {
                    return Err("usage: me KEY [b]".into());
                }
                validate_meta_key(key, !tail.is_empty())?;
                return meta(MetaCommand::Debug {
                    key: key.clone(),
                    binary: !tail.is_empty(),
                });
            }
            let flags = parse_meta_flags(verb, tail)?;
            validate_meta_key(
                key,
                flags.iter().any(|flag| matches!(flag, MetaFlag::BinaryKey)),
            )?;
            match verb.as_str() {
                "mg" => meta(MetaCommand::Get {
                    key: key.clone(),
                    flags,
                }),
                "md" => meta(MetaCommand::Delete {
                    key: key.clone(),
                    flags,
                }),
                _ => meta(MetaCommand::Arithmetic {
                    key: key.clone(),
                    flags,
                }),
            }
        }
        "ms" => {
            let (key, args) = args.split_first().ok_or("usage: ms KEY VALUE [FLAGS...]")?;
            let (value, tail) = parse_value(args)?;
            let flags = parse_meta_flags("ms", tail)?;
            validate_meta_key(
                key,
                flags.iter().any(|flag| matches!(flag, MetaFlag::BinaryKey)),
            )?;
            meta(MetaCommand::Set {
                key: key.clone(),
                value,
                flags,
            })
        }
        "mn" => {
            exactly(args, 0, "mn")?;
            meta(MetaCommand::Noop)
        }
        "inspect" => {
            if args.is_empty()
                || args.len() > 2
                || (args.len() == 2 && args[1] != "--value" && args[1] != "value")
            {
                return Err("usage: inspect KEY [value | --value]".into());
            }
            validate_key(&args[0])?;
            let mut flags = vec![
                MetaFlag::ClientFlags,
                MetaFlag::Cas,
                MetaFlag::RemainingTtl,
                MetaFlag::Size,
                MetaFlag::Hit,
                MetaFlag::LastAccess,
            ];
            if args.len() == 2 {
                flags.push(MetaFlag::Value);
            }
            meta(MetaCommand::Get {
                key: args[0].clone(),
                flags,
            })
        }
        "help" => {
            let topic = match args {
                [] => None,
                [command] => Some(command.clone()),
                [command, subcommand]
                    if (command == "stats"
                        && matches!(
                            subcommand.as_str(),
                            "items" | "slabs" | "settings" | "sizes"
                        ))
                        || (command == "recent"
                            && matches!(subcommand.as_str(), "forget" | "clear")) =>
                {
                    Some(format!("{command} {subcommand}"))
                }
                _ => return Err("usage: help [command]".into()),
            };
            local(LocalCommand::Help(topic))
        }
        "history" => {
            exactly(args, 0, "history")?;
            local(LocalCommand::History)
        }
        "recent" => {
            let subcommand = match args.first().map(String::as_str) {
                None => RecentCommand::List,
                Some("clear") if args.len() == 1 => RecentCommand::Clear,
                Some("forget")
                    if args.len() == 2
                        || (args.len() == 3 && matches!(args[2].as_str(), "--tls" | "tls")) =>
                {
                    if args[1].is_empty()
                        || args[1].chars().any(|c| c.is_whitespace() || c.is_control())
                    {
                        return Err("recent forget requires a host address".into());
                    }
                    RecentCommand::Forget {
                        address: args[1].clone(),
                        tls: (args.len() == 3).then_some(true),
                    }
                }
                _ => return Err("usage: recent [forget HOST:PORT [tls | --tls]|clear]".into()),
            };
            local(LocalCommand::Recent(subcommand))
        }
        "reconnect" => {
            exactly(args, 0, "reconnect")?;
            local(LocalCommand::Reconnect)
        }
        "quit" => {
            exactly(args, 0, "quit")?;
            local(LocalCommand::Quit)
        }
        "exit" => {
            exactly(args, 0, "exit")?;
            local(LocalCommand::Exit)
        }
        _ => Err(format!("unknown command {verb}; try help")),
    }
}

fn exactly(args: &[String], expected: usize, usage: &str) -> Result<(), String> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(format!("usage: {usage}"))
    }
}

fn validate_multi_key_line(prefix_bytes: usize, keys: &[String]) -> Result<(), String> {
    let length = prefix_bytes + keys.iter().map(|key| 1 + key.len()).sum::<usize>() + 2;
    if length > 8192 {
        Err("request line exceeds 8 KiB limit".into())
    } else {
        Ok(())
    }
}

fn option_value<'a>(
    option: &'a str,
    rest: &'a [String],
    missing: &str,
) -> Result<(&'a str, &'a [String]), String> {
    if let Some((_, value)) = option.split_once('=') {
        return Ok((value, rest));
    }
    let (value, remaining) = rest
        .split_first()
        .ok_or_else(|| format!("{option} {missing}"))?;
    Ok((value, remaining))
}

fn decimal<T: std::str::FromStr>(input: &str, name: &str) -> Result<T, String> {
    if input.is_empty() || !input.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{name} must be an unsigned decimal number"));
    }
    input.parse().map_err(|_| format!("{name} is out of range"))
}

fn relative_ttl(input: &str) -> Result<u32, String> {
    let ttl = decimal::<u32>(input, "relative TTL")?;
    if ttl > MAX_RELATIVE_TTL {
        return Err(format!(
            "relative TTL must be at most {MAX_RELATIVE_TTL} seconds (30 days)"
        ));
    }
    Ok(ttl)
}

fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > 250 || key.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(
            "key must be 1..=250 UTF-8 bytes without whitespace or control characters".into(),
        );
    }
    Ok(())
}

fn validate_meta_key(key: &str, binary: bool) -> Result<(), String> {
    if !binary {
        return validate_key(key);
    }
    // The wire token may exceed 250 bytes: the decoded binary key is what is limited.
    if key.is_empty() || key.len() > 4 * 250_usize.div_ceil(3) {
        return Err("base64 meta key exceeds the 250-byte decoded-key limit".into());
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(key)
        .map_err(|error| format!("invalid base64 meta key: {error}"))?;
    if decoded.is_empty() || decoded.len() > 250 {
        return Err("decoded meta key must be 1..=250 bytes".into());
    }
    Ok(())
}

fn open_regular_value(path: &str) -> Result<File, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("cannot read value file {path}: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("value source must be a regular file: {path}"));
    }
    if metadata.len() > MAX_VALUE_BYTES as u64 {
        return Err("value exceeds 16 MiB limit".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("cannot read value file {path}: {error}"))?;
    if !file
        .metadata()
        .map_err(|error| format!("cannot inspect value file {path}: {error}"))?
        .is_file()
    {
        return Err(format!("value source must be a regular file: {path}"));
    }
    Ok(file)
}

/// Consume exactly one inline, base64, or file value. File I/O occurs here, before any request.
fn parse_value(args: &[String]) -> Result<(Vec<u8>, &[String]), String> {
    let (first, tail) = args
        .split_first()
        .ok_or("missing value (use VALUE, --base64 TEXT, or --file PATH)")?;
    let option = first
        .split_once('=')
        .map_or(first.as_str(), |(name, _)| name);
    let (value, tail) = match option {
        "--base64" => {
            let (text, rest) = option_value(first, tail, "requires encoded text")?;
            if text.len() > (MAX_VALUE_BYTES / 3 + 1) * 4 {
                return Err("base64 value exceeds 16 MiB decoded limit".into());
            }
            (
                base64::engine::general_purpose::STANDARD
                    .decode(text)
                    .map_err(|error| format!("invalid base64 value: {error}"))?,
                rest,
            )
        }
        "--file" => {
            let (path, rest) = option_value(first, tail, "requires a path")?;
            let file = open_regular_value(path)?;
            let mut value = Vec::new();
            file.take((MAX_VALUE_BYTES + 1) as u64)
                .read_to_end(&mut value)
                .map_err(|error| format!("cannot read value file {path}: {error}"))?;
            (value, rest)
        }
        option if option.starts_with("--") => return Err(format!("unknown value option {option}")),
        _ => (first.as_bytes().to_vec(), tail),
    };
    if value.len() > MAX_VALUE_BYTES {
        return Err("value exceeds 16 MiB limit".into());
    }
    Ok((value, tail))
}

fn parse_meta_flags(verb: &str, tokens: &[String]) -> Result<Vec<MetaFlag>, String> {
    let mut seen = HashSet::new();
    let mut flags = Vec::with_capacity(tokens.len());
    for token in tokens {
        let code_end = token.chars().next().map(char::len_utf8).unwrap_or(0);
        let (code, argument) = token.split_at(code_end);
        if !seen.insert(code) {
            return Err(format!("duplicate {verb} flag {code}"));
        }
        if code == "q" {
            return Err("meta quiet flag q is unsupported: it suppresses responses and desynchronizes interactive requests".into());
        }
        let flag = match (verb, code) {
            (_, "b") => bare(argument, MetaFlag::BinaryKey, code)?,
            ("mg" | "ms" | "ma", "c") => bare(argument, MetaFlag::Cas, code)?,
            (_, "C") => MetaFlag::CompareCas(decimal(argument, "CAS ID")?),
            ("mg", "f") => bare(argument, MetaFlag::ClientFlags, code)?,
            ("mg", "h") => bare(argument, MetaFlag::Hit, code)?,
            (_, "k") => bare(argument, MetaFlag::Key, code)?,
            ("mg", "l") => bare(argument, MetaFlag::LastAccess, code)?,
            (_, "O") => {
                if argument.is_empty()
                    || argument.len() > 32
                    || !argument.bytes().all(|b| b.is_ascii_graphic())
                {
                    return Err("opaque O token must contain 1..=32 printable ASCII bytes".into());
                }
                MetaFlag::Opaque(argument.into())
            }
            ("mg" | "ms", "s") => bare(argument, MetaFlag::Size, code)?,
            ("mg" | "ma", "t") => bare(argument, MetaFlag::RemainingTtl, code)?,
            ("mg", "u") => bare(argument, MetaFlag::NoLruBump, code)?,
            ("mg" | "ma", "v") => bare(argument, MetaFlag::Value, code)?,
            ("mg" | "ms" | "md" | "ma", "E") => MetaFlag::OverrideCas(decimal(argument, "CAS ID")?),
            ("mg" | "ms" | "ma", "N") => MetaFlag::Vivify(relative_ttl(argument)?),
            ("mg", "R") => MetaFlag::Recache(relative_ttl(argument)?),
            (_, "T") => MetaFlag::Ttl(relative_ttl(argument)?),
            ("ms", "F") => MetaFlag::SetClientFlags(decimal(argument, "client flags")?),
            ("ms" | "md", "I") => bare(argument, MetaFlag::Invalidate, code)?,
            ("ms" | "ma", "M") => {
                if argument.len() != 1
                    || !(if verb == "ms" { "ERAPS" } else { "I+D-" }).contains(argument)
                {
                    return Err(format!("unsupported {verb} mode M{argument}"));
                }
                MetaFlag::Mode(argument.as_bytes()[0] as char)
            }
            ("ma", "J") => MetaFlag::Initial(decimal(argument, "initial value")?),
            ("ma", "D") => MetaFlag::Delta(decimal(argument, "delta")?),
            ("md", "x") => bare(argument, MetaFlag::RemoveValue, code)?,
            _ => return Err(format!("unsupported {verb} flag {token}")),
        };
        flags.push(flag);
    }
    let has = |predicate: fn(&MetaFlag) -> bool| flags.iter().any(predicate);
    match verb {
        "mg" if has(|f| matches!(f, MetaFlag::OverrideCas(_)))
            && !has(|f| {
                matches!(
                    f,
                    MetaFlag::Vivify(_) | MetaFlag::Recache(_) | MetaFlag::Ttl(_)
                )
            }) =>
        {
            return Err("mg E requires N, R, or T to modify an item".into());
        }
        "ms" if has(|f| matches!(f, MetaFlag::Invalidate))
            && !has(|f| matches!(f, MetaFlag::CompareCas(_))) =>
        {
            return Err("ms I requires C<ID>".into());
        }
        "ms" if has(|f| matches!(f, MetaFlag::Vivify(_)))
            && !has(|f| matches!(f, MetaFlag::Mode('A'))) =>
        {
            return Err("ms N<TTL> requires append mode MA".into());
        }
        "md" if has(|f| matches!(f, MetaFlag::Ttl(_)))
            && !has(|f| matches!(f, MetaFlag::Invalidate)) =>
        {
            return Err("md T<TTL> requires I".into());
        }
        "ma" if has(|f| matches!(f, MetaFlag::Initial(_)))
            && !has(|f| matches!(f, MetaFlag::Vivify(_))) =>
        {
            return Err("ma J<N> requires N<TTL>".into());
        }
        _ => {}
    }
    Ok(flags)
}

fn bare(argument: &str, flag: MetaFlag, code: &str) -> Result<MetaFlag, String> {
    if argument.is_empty() {
        Ok(flag)
    } else {
        Err(format!("flag {code} does not accept an argument"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(input: &str) -> Command {
        parse(input).unwrap()
    }
    fn invalid(input: &str) {
        assert!(parse(input).is_err(), "unexpectedly accepted: {input}");
    }

    #[test]
    fn basic_forms_and_bounds() {
        assert!(
            matches!(parsed("get a b"), Command::Basic(BasicCommand::Get { keys, cas: false }) if keys == ["a", "b"])
        );
        assert!(matches!(
            parsed("gets a"),
            Command::Basic(BasicCommand::Get { cas: true, .. })
        ));
        assert!(matches!(
            parsed("gat 30 a"),
            Command::Basic(BasicCommand::Gat {
                ttl: 30,
                cas: false,
                ..
            })
        ));
        assert!(matches!(
            parsed("gats 0 a"),
            Command::Basic(BasicCommand::Gat { cas: true, .. })
        ));
        for verb in ["set", "add", "replace", "append", "prepend"] {
            assert!(
                matches!(parsed(&format!("{verb} a ''")), Command::Basic(BasicCommand::Store { value, .. }) if value.is_empty())
            );
        }
        assert!(matches!(
            parsed("cas a v --cas 18446744073709551615 --ttl 2592000 --flags 4294967295"),
            Command::Basic(BasicCommand::Store {
                cas: Some(u64::MAX),
                ttl: MAX_RELATIVE_TTL,
                flags: u32::MAX,
                ..
            })
        ));
        for form in [
            "delete a",
            "incr a 0",
            "decr a 18446744073709551615",
            "touch a --ttl 0",
            "stats",
            "stats sizes",
            "version",
            "flush_all --delay 30",
        ] {
            parsed(form);
        }
        for form in [
            "gat 2592001 a",
            "set a v --ttl 2592001",
            "set a v --ttl -1",
            "touch a --ttl 2592001",
            "flush_all --delay 2592001",
            "set a v --flags 4294967296",
            "incr a 18446744073709551616",
            "cas a v",
            "cas a v --cas -1",
            "append a v --ttl 3",
            "prepend a v --flags 1",
            "set a v --cas 2",
            "set a v --ttl 1 --ttl 2",
            "stats reset",
            "stats sizes_enable",
            "stats items more",
            "get",
        ] {
            invalid(form);
        }
    }

    #[test]
    fn positional_and_named_forms_encode_identically() {
        for (positional, named) in [
            ("set k v 0", "set k v --ttl 0"),
            ("set k v 30 7", "set k v --ttl 30 --flags 7"),
            (
                "add k v 2592000 4294967295",
                "add k v --ttl 2592000 --flags 4294967295",
            ),
            ("replace k v 1 2", "replace k v --flags 2 --ttl 1"),
            ("cas k v 0", "cas k v --cas 0"),
            (
                "cas k v 18446744073709551615 2592000 4294967295",
                "cas k v --cas 18446744073709551615 --ttl 2592000 --flags 4294967295",
            ),
            ("touch k 30", "touch k --ttl 30"),
            ("flush_all 30", "flush_all --delay 30"),
            ("inspect k value", "inspect k --value"),
            (
                "recent forget cache:11211 tls",
                "recent forget cache:11211 --tls",
            ),
        ] {
            let command = parsed(positional);
            assert_eq!(command, parsed(named), "{positional}");
            if let Command::Basic(basic) = command {
                let Command::Basic(named) = parsed(named) else {
                    unreachable!()
                };
                assert_eq!(
                    crate::request::basic(&basic).unwrap().bytes,
                    crate::request::basic(&named).unwrap().bytes,
                    "{positional}"
                );
            }
        }
        assert_eq!(parsed("flush_all 0"), parsed("flush_all --delay 0"));
        assert_eq!(
            parsed("recent forget cache:11211"),
            Command::Local(LocalCommand::Recent(RecentCommand::Forget {
                address: "cache:11211".into(),
                tls: None,
            }))
        );
        assert!(parsed("get k v 30").is_readonly());
        assert!(matches!(
            parsed("gets k v"),
            Command::Basic(BasicCommand::Get { keys, cas: true }) if keys == ["k", "v"]
        ));
    }

    #[test]
    fn valued_options_accept_equals_without_changing_their_meaning() {
        let set = parsed("set test3 tesst --flags=123");
        assert!(matches!(
            set,
            Command::Basic(BasicCommand::Store { flags: 123, .. })
        ));
        assert_eq!(set, parsed("set test3 tesst --flags 123"));
        assert_eq!(
            parsed("cas k v --cas=42 --ttl=30 --flags=7"),
            parsed("cas k v --cas 42 --ttl 30 --flags 7")
        );
        assert_eq!(parsed("touch k --ttl=30"), parsed("touch k --ttl 30"));
        assert_eq!(
            parsed("flush_all --delay=30"),
            parsed("flush_all --delay 30")
        );
        assert_eq!(
            parsed("set k --base64=dGVzdA== --flags=7"),
            parsed("set k test --flags 7")
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("value");
        std::fs::write(&path, b"test").unwrap();
        assert_eq!(
            parsed(&format!("set k --file={} --flags=7", path.display())),
            parsed("set k test --flags 7")
        );
        invalid("set k v --flags=4294967296");
        invalid("set k v --flags=7 --flags 8");
        invalid("set k v --ttl=1 2");
        assert!(
            parse("set k v --flag=123")
                .unwrap_err()
                .contains("unsupported option --flag")
        );
    }

    #[test]
    fn positional_bounds_and_ambiguous_mixtures_reject() {
        for form in [
            "set k v 2592001",
            "set k v -1",
            "set k v 1 4294967296",
            "set k v 4294967296",
            "set k v 1 2 3",
            "add k v 1 --flags 2",
            "replace k v --ttl 1 2",
            "set k v 1 --ttl 2",
            "set k v --flags 1 2",
            "set k v --flags 1 --flags 2",
            "append k v 1",
            "prepend k v 1 2",
            "cas k v 18446744073709551616",
            "cas k v -1",
            "cas k v 1 2592001",
            "cas k v 1 2 4294967296",
            "cas k v 1 2 3 4",
            "cas k v 1 --ttl 2",
            "cas k v --cas 1 2",
            "cas k v --cas 1 --cas 2",
            "touch k 2592001",
            "touch k 1 --ttl 2",
            "flush_all 2592001",
            "flush_all 1 --delay 2",
            "inspect k value --value",
            "recent forget cache:11211 tls --tls",
        ] {
            invalid(form);
        }
    }

    #[test]
    fn shell_quoting_and_binary_payloads() {
        assert!(
            matches!(parsed(r#"set key "hello world""#), Command::Basic(BasicCommand::Store { value, .. }) if value == b"hello world")
        );
        assert!(
            matches!(parsed(r"set key hello\ world"), Command::Basic(BasicCommand::Store { value, .. }) if value == b"hello world")
        );
        assert!(
            matches!(parsed("set key --base64 AP8NCg=="), Command::Basic(BasicCommand::Store { value, .. }) if value == [0, 255, 13, 10])
        );
        assert_eq!(
            parsed("set key --base64 AP8NCg== 30 7"),
            parsed("set key --base64 AP8NCg== --ttl 30 --flags 7")
        );
        assert_eq!(
            parsed("cas key --base64 AP8NCg== 42 30 7"),
            parsed("cas key --base64 AP8NCg== --cas 42 --ttl 30 --flags 7")
        );
        invalid("set key --base64 AP8NCg== 30 --flags 7");
        assert!(
            matches!(parsed("ms key --base64 AA== T60"), Command::Meta(MetaCommand::Set { value, .. }) if value == [0])
        );
        invalid("set key --base64 ?");
        invalid("set key --file");
        invalid("set key value --base64 AA==");
        invalid("set key 'unterminated");
        invalid("set key hello world");
    }

    #[test]
    fn file_value_is_binary_and_bounded() {
        let path = std::env::temp_dir().join(format!("mctl-command-value-{}", std::process::id()));
        std::fs::write(&path, [0, 255, 13, 10]).unwrap();
        let input = format!("ms key --file '{}' T30", path.display());
        assert!(
            matches!(parsed(&input), Command::Meta(MetaCommand::Set { value, .. }) if value == [0, 255, 13, 10])
        );
        let positional = format!("cas key --file '{}' 42 30 7", path.display());
        let named = format!(
            "cas key --file '{}' --cas 42 --ttl 30 --flags 7",
            path.display()
        );
        assert_eq!(parsed(&positional), parsed(&named));
        invalid(&format!("set key --file '{}' 30 --flags 7", path.display()));
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_VALUE_BYTES as u64 + 1)
            .unwrap();
        invalid(&input);
        invalid(&positional);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn utf8_and_binary_key_limits() {
        assert!(parse(&format!("get {}", "é".repeat(125))).is_ok());
        invalid(&format!("get {}", "é".repeat(126)));
        for input in [
            "get 'two words'",
            "get 'bad\tkey'",
            "get ''",
            "mg key b",
            "mg YQ== b x",
            "mg key é",
        ] {
            invalid(input);
        }
        assert!(
            matches!(parsed("mg AP8= b v"), Command::Meta(MetaCommand::Get { key, .. }) if key == "AP8=")
        );
        invalid("mg AA b");
        let encoded = base64::engine::general_purpose::STANDARD.encode([255; 250]);
        assert!(parse(&format!("mg {encoded} b")).is_ok());
        let too_long = base64::engine::general_purpose::STANDARD.encode([255; 251]);
        invalid(&format!("mg {too_long} b"));
        invalid(&format!("get {}", vec!["x".repeat(250); 33].join(" ")));
    }

    #[test]
    fn meta_flag_classification_and_constraints() {
        for form in ["me key", "me YQ== b", "mn"] {
            assert!(parsed(form).is_readonly(), "{form}");
        }
        for form in [
            "mg key f c t s v",
            "mg YQ== b C1 Otag h l k u",
            "inspect key",
            "inspect key --value",
            "mg key N30",
            "mg key R30",
            "mg key T60",
            "mg key N30 E1",
            "ms key v T60 F1 ME",
            "ms key v C1 I",
            "md key I T30",
            "md key x",
            "ma key D2 M+ v",
            "ma key N30 J7",
            "gat 2 key",
            "gats 2 key",
        ] {
            assert!(!parsed(form).is_readonly(), "{form}");
        }
        for form in [
            "mg key q",
            "mg key Q",
            "mg key T2592001",
            "mg key N2592001",
            "mg key R2592001",
            "mg key E1",
            "mg key bplus",
            "mg key f f",
            "mg key O",
            "mg key Oabcdefghijklmnopqrstuvwxyz1234567",
            "ms key v Mx",
            "ms key v N3",
            "ms key v I",
            "ms key v F4294967296",
            "md key T30",
            "ma key J2",
            "ma key D-1",
            "ma key q",
            "me key v",
            "mn extra",
            "inspect key --raw",
        ] {
            invalid(form);
        }
        assert_eq!(MetaFlag::Mode('+').as_token(), "M+");
        let Command::Meta(MetaCommand::Get { flags, .. }) = parsed("inspect key --value") else {
            panic!("inspect must expand to mg")
        };
        assert_eq!(
            flags,
            [
                MetaFlag::ClientFlags,
                MetaFlag::Cas,
                MetaFlag::RemainingTtl,
                MetaFlag::Size,
                MetaFlag::Hit,
                MetaFlag::LastAccess,
                MetaFlag::Value
            ]
        );
        assert!(!parsed("inspect key").is_readonly());
    }

    #[test]
    fn readonly_allowlist_covers_all_basic_categories() {
        for form in [
            "get key",
            "gets key",
            "stats",
            "stats items",
            "stats slabs",
            "stats settings",
            "stats sizes",
            "version",
        ] {
            assert!(parsed(form).is_readonly(), "{form}");
        }
        for form in [
            "set k v",
            "add k v",
            "replace k v",
            "append k v",
            "prepend k v",
            "cas k v --cas 1",
            "delete k",
            "incr k 1",
            "decr k 1",
            "touch k --ttl 30",
            "gat 30 k",
            "gats 30 k",
            "flush_all",
        ] {
            let command = parsed(form);
            assert!(!command.is_readonly(), "{form}");
            assert!(command.readonly_error().is_some(), "{form}");
        }
    }

    #[test]
    fn local_readonly_rules() {
        for form in [
            "help",
            "help mg",
            "help stats sizes",
            "help stats items",
            "help stats slabs",
            "help stats settings",
            "help recent forget",
            "help recent clear",
            "history",
            "recent",
            "recent forget cache:11211",
            "recent forget cache:11211 --tls",
            "recent clear",
            "reconnect",
            "quit",
            "exit",
        ] {
            assert!(parsed(form).is_readonly());
        }
        for form in [
            "recent forget",
            "recent clear extra",
            "help stats unsupported",
            "help recent unsupported",
            "help set extra",
            "recent forget cache:11211 --plain",
            "quit now",
            "touch k --ttl 0 extra",
            "unknown key",
        ] {
            invalid(form);
        }
    }
    #[test]
    fn readonly_rejects_incomplete_mutations_before_argument_validation() {
        for form in [
            "set test",
            "cas k",
            "touch k",
            "flush_all --delay",
            "mg",
            "set k --file /does/not/exist",
        ] {
            let error = parse_for_session(form, true).unwrap_err();
            assert!(error.starts_with("readonly:"), "{form}: {error}");
        }
        for form in ["get k", "stats", "help set", "recent", "version"] {
            assert!(parse_for_session(form, true).is_ok(), "{form}");
        }
        assert!(
            parse_for_session("get", true)
                .unwrap_err()
                .starts_with("usage:")
        );
        assert!(
            parse_for_session("unknown k", true)
                .unwrap_err()
                .starts_with("unknown command")
        );
    }

    #[test]
    fn readonly_preflight_agrees_with_typed_policy_for_mutations() {
        for form in [
            "set k v",
            "add k v",
            "replace k v",
            "append k v",
            "prepend k v",
            "cas k v 1",
            "delete k",
            "incr k 1",
            "decr k 1",
            "touch k 30",
            "gat 30 k",
            "gats 30 k",
            "flush_all",
            "mg k",
            "inspect k",
            "ms k v",
            "md k",
            "ma k",
        ] {
            assert_eq!(
                parse_for_session(form, true).unwrap_err(),
                parsed(form).readonly_error().unwrap(),
                "{form}"
            );
        }
    }
}
