use base64::Engine as _;
use nu_ansi_term::{Color, Style};

use crate::command::{BasicCommand, Command};
use crate::wire::{Item, WireResponse};

const DISPLAY_LIMIT: usize = 4096;

pub fn response(command: &Command, result: &WireResponse, color: bool) -> String {
    match result {
        WireResponse::Values(items) => {
            let requested = match command {
                Command::Basic(BasicCommand::Get { keys, .. } | BasicCommand::Gat { keys, .. }) => {
                    keys
                }
                _ => return "error: unexpected value response".into(),
            };
            requested
                .iter()
                .map(|key| {
                    let label = paint(&escape(key), Color::Cyan.normal(), color);
                    match items.iter().find(|item| item.key == key.as_bytes()) {
                        Some(item) => format!("{label}  {}", format_item(item)),
                        None => {
                            format!("{label}  {}", paint("(nil)", Color::Yellow.normal(), color))
                        }
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        WireResponse::Stats(entries) => entries
            .iter()
            .map(|(name, value)| format!("STAT {} {}", escape(name), escape(value)))
            .collect::<Vec<_>>()
            .join("\n"),
        WireResponse::Meta {
            code,
            tokens,
            value,
        } => {
            let style = match code.as_str() {
                "HD" | "VA" | "ME" | "MN" => Color::Green.normal(),
                "EN" | "NF" | "NS" | "EX" => Color::Yellow.normal(),
                _ => Color::Red.normal(),
            };
            let mut output = paint(&escape(code), style, color);
            for token in tokens {
                output.push(' ');
                output.push_str(&escape(token));
            }
            if let Some(value) = value {
                output.push_str("  ");
                output.push_str(&display_value(value));
                output.push_str(&format!(" (bytes={})", value.len()));
            }
            output
        }
        WireResponse::Status(status) => {
            let status = escape(status);
            if status.starts_with("ERROR")
                || status.starts_with("CLIENT_ERROR")
                || status.starts_with("SERVER_ERROR")
            {
                format!("error: {}", paint(&status, Color::Red.normal(), color))
            } else {
                let style = if matches!(status.as_str(), "NOT_STORED" | "NOT_FOUND" | "EXISTS") {
                    Color::Yellow.normal()
                } else {
                    Color::Green.normal()
                };
                paint(&status, style, color)
            }
        }
        WireResponse::Version(version) => format!("VERSION {}", escape(version)),
    }
}

pub fn error(message: &str, color: bool) -> String {
    format!(
        "error: {}",
        paint(&escape(message), Color::Red.normal(), color)
    )
}

fn format_item(item: &Item) -> String {
    let mut output = display_value(&item.value);
    output.push_str(&format!(
        " (flags={}, bytes={}",
        item.flags,
        item.value.len()
    ));
    if let Some(cas) = item.cas {
        output.push_str(&format!(", cas={cas}"));
    }
    output.push(')');
    output
}

fn display_value(value: &[u8]) -> String {
    let mut text = match std::str::from_utf8(value) {
        Ok(utf8) => {
            let mut end = utf8.len().min(DISPLAY_LIMIT);
            while !utf8.is_char_boundary(end) {
                end -= 1;
            }
            format!("\"{}\"", escape(&utf8[..end]))
        }
        Err(_) => format!(
            "base64:{}",
            base64::engine::general_purpose::STANDARD
                .encode(&value[..value.len().min(DISPLAY_LIMIT)])
        ),
    };
    if value.len() > DISPLAY_LIMIT {
        text.push_str(&format!(" (truncated; {} bytes total)", value.len()));
    }
    text
}

pub fn escape(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\0' => result.push_str("\\0"),
            control if control.is_control() => {
                result.push_str(&format!("\\u{{{:x}}}", control as u32))
            }
            other => result.push(other),
        }
    }
    result
}

fn paint(text: &str, style: Style, color: bool) -> String {
    if color {
        style.paint(text).to_string()
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::parse;

    #[test]
    fn preserves_order_and_marks_each_missing_key() {
        let command = parse("gets missing found absent").unwrap();
        let result = WireResponse::Values(vec![Item {
            key: b"found".to_vec(),
            flags: 3,
            cas: Some(9),
            value: b"ok".to_vec(),
        }]);
        assert_eq!(
            response(&command, &result, false),
            "missing  (nil)\nfound  \"ok\" (flags=3, bytes=2, cas=9)\nabsent  (nil)"
        );
    }

    #[test]
    fn server_bytes_cannot_inject_terminal_sequences() {
        let command = parse("get key").unwrap();
        let result = WireResponse::Values(vec![Item {
            key: b"key".to_vec(),
            flags: 0,
            cas: None,
            value: b"\x1b[31m\r\n\0".to_vec(),
        }]);
        let rendered = response(&command, &result, false);
        assert!(!rendered.contains('\x1b'));
        assert!(rendered.contains("\\u{1b}[31m\\r\\n\\0"));
        assert_eq!(display_value(&[0xff, 0x00]), "base64:/wA=");
        assert_eq!(
            response(
                &command,
                &WireResponse::Status("SERVER_ERROR bad\x1b[2J".into()),
                false
            ),
            "error: SERVER_ERROR bad\\u{1b}[2J"
        );
    }

    #[test]
    fn meta_status_keeps_requested_and_unknown_tokens() {
        let command = parse("mg key t v").unwrap();
        let result = WireResponse::Meta {
            code: "VA".into(),
            tokens: vec!["t-1".into(), "X".into(), "W".into(), "znew".into()],
            value: Some(b"hello".to_vec()),
        };
        assert_eq!(
            response(&command, &result, false),
            "VA t-1 X W znew  \"hello\" (bytes=5)"
        );
    }

    #[test]
    fn truncates_display_only_after_reading_full_value() {
        let mut value = vec![b'a'; DISPLAY_LIMIT + 3];
        value[DISPLAY_LIMIT] = b'!';
        let rendered = display_value(&value);
        assert!(rendered.contains("truncated; 4099 bytes total"));
        assert!(!rendered.contains('!'));
    }
}
