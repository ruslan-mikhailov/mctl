//! Scrolling line editor: command hints are ranked independently of session history.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nu_ansi_term::{Color, Style};
use parking_lot::Mutex;
use reedline::{
    ColumnarMenu, Completer, CompletionResult, Emacs, Highlighter, Hinter, History, KeyCode,
    KeyModifiers, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch, Reedline,
    ReedlineEvent, ReedlineMenu, Span, StyledText, Suggestion, default_emacs_keybindings,
};

const COMMANDS: &[&str] = &[
    "get",
    "gets",
    "stats",
    "version",
    "inspect",
    "mg",
    "me",
    "mn",
    "help",
    "history",
    "recent",
    "reconnect",
    "quit",
    "exit",
    "set",
    "add",
    "replace",
    "append",
    "prepend",
    "cas",
    "delete",
    "incr",
    "decr",
    "touch",
    "gat",
    "gats",
    "flush_all",
    "ms",
    "md",
    "ma",
];
const READONLY_COMMANDS: &[&str] = &[
    "get",
    "gets",
    "stats",
    "version",
    "me",
    "mn",
    "help",
    "history",
    "recent",
    "reconnect",
    "quit",
    "exit",
];
const STATS: &[&str] = &["items", "slabs", "settings", "sizes"];
const MG_SAFE_FLAGS: &[&str] = &["b", "c", "f", "h", "k", "l", "s", "t", "u", "v"];
const MS_FLAGS: &[&str] = &["b", "c", "k", "s", "I", "ME", "MR", "MA", "MP", "MS"];
const MD_FLAGS: &[&str] = &["b", "k", "I", "x"];
const MA_FLAGS: &[&str] = &["b", "c", "k", "t", "v", "MI", "M+", "M-"];

fn commands(readonly: bool) -> &'static [&'static str] {
    if readonly {
        READONLY_COMMANDS
    } else {
        COMMANDS
    }
}

struct CommandHinter {
    readonly: bool,
    suffix: String,
}

impl CommandHinter {
    fn new(readonly: bool) -> Self {
        Self {
            readonly,
            suffix: String::new(),
        }
    }
}

impl Hinter for CommandHinter {
    fn handle(
        &mut self,
        line: &str,
        pos: usize,
        _history: &dyn History,
        use_ansi_coloring: bool,
        _cwd: &str,
    ) -> String {
        self.suffix.clear();
        // Only a partially typed verb at the end of the buffer gets a ghost suffix.
        if pos == line.len()
            && !line.is_empty()
            && !line.chars().any(char::is_whitespace)
            && !commands(self.readonly).contains(&line)
        {
            if let Some(command) = commands(self.readonly)
                .iter()
                .find(|command| command.starts_with(line))
            {
                self.suffix.push_str(&command[line.len()..]);
                self.suffix.push(' ');
            }
        }
        if use_ansi_coloring && !self.suffix.is_empty() {
            Style::new()
                .dimmed()
                .fg(Color::LightGray)
                .paint(&self.suffix)
                .to_string()
        } else {
            self.suffix.clone()
        }
    }

    fn complete_hint(&self) -> String {
        self.suffix.clone()
    }

    fn next_hint_token(&self) -> String {
        self.suffix.clone()
    }
}

struct CommandHighlighter {
    readonly: bool,
}

impl Highlighter for CommandHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled = StyledText::new();
        let end = line.find(char::is_whitespace).unwrap_or(line.len());
        if end > 0
            && commands(self.readonly)
                .iter()
                .any(|command| command.starts_with(&line[..end]))
        {
            styled.push((Style::new().bold().fg(Color::Blue), line[..end].to_owned()));
            styled.push((Style::new(), line[end..].to_owned()));
        } else {
            styled.push((Style::new(), line.to_owned()));
        }
        styled
    }
}

struct CommandCompleter {
    readonly: bool,
    known_keys: Arc<Mutex<Vec<String>>>,
}

impl Completer for CommandCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        if pos > line.len() || !line.is_char_boundary(pos) {
            return CompletionResult::fresh(Vec::<Suggestion>::new());
        }
        let start = line[..pos]
            .char_indices()
            .rfind(|(_, ch)| ch.is_whitespace())
            .map_or(0, |(index, ch)| index + ch.len_utf8());
        let end = line[pos..]
            .find(char::is_whitespace)
            .map_or(line.len(), |i| pos + i);
        if line[start..end]
            .bytes()
            .any(|b| matches!(b, b'\'' | b'"' | b'\\'))
        {
            return CompletionResult::fresh(Vec::<Suggestion>::new());
        }
        let Ok(before) = shell_words::split(&line[..start]) else {
            return CompletionResult::fresh(Vec::<Suggestion>::new());
        };
        let prefix = &line[start..pos];
        let index = before.len();
        let verb = before.first().map(String::as_str).unwrap_or("");
        let value_end = if matches!(
            before.get(2).map(String::as_str),
            Some("--file" | "--base64")
        ) {
            4
        } else {
            3
        };
        let span = Span::new(start, end);
        let mut suggestions = Vec::new();
        let mut push = |candidate: &str, trailing_space: bool| {
            if candidate.starts_with(prefix) && (candidate != prefix || trailing_space) {
                suggestions.push(Suggestion {
                    value: candidate.to_owned(),
                    span,
                    append_whitespace: trailing_space,
                    ..Suggestion::default()
                });
            }
        };

        if index == 0 {
            for command in commands(self.readonly) {
                push(command, true);
            }
        } else if !commands(self.readonly).contains(&verb) {
            // Do not suggest arguments to a disallowed or unknown command.
        } else if verb == "stats" && index == 1 {
            for candidate in STATS {
                push(candidate, false);
            }
        } else if verb == "help" && index == 1 {
            for candidate in commands(self.readonly) {
                push(candidate, false);
            }
        } else if verb == "recent" {
            if !self.readonly && index == 1 {
                push("forget", true);
                push("clear", false);
            } else if !self.readonly
                && index == 3
                && before.get(1).is_some_and(|part| part == "forget")
            {
                push("--tls", false);
            }
        } else if ((verb == "get" || verb == "gets") && index >= 1)
            || ((verb == "gat" || verb == "gats") && index >= 2)
            || (matches!(
                verb,
                "inspect"
                    | "mg"
                    | "me"
                    | "ms"
                    | "md"
                    | "ma"
                    | "set"
                    | "add"
                    | "replace"
                    | "append"
                    | "prepend"
                    | "cas"
                    | "delete"
                    | "incr"
                    | "decr"
                    | "touch"
            ) && index == 1)
        {
            let keys = self.known_keys.lock();
            for key in keys.iter() {
                if key.starts_with(prefix) && key != prefix {
                    // Shell quoting protects a previously seen key with quote characters.
                    let value = if key.bytes().any(|b| matches!(b, b'\'' | b'"' | b'\\')) {
                        format!("\"{}\"", key.replace('\\', "\\\\").replace('"', "\\\""))
                    } else {
                        key.clone()
                    };
                    suggestions.push(Suggestion {
                        value,
                        span,
                        append_whitespace: true,
                        ..Suggestion::default()
                    });
                }
            }
        } else if verb == "inspect" && index == 2 {
            push("--value", false);
        } else if verb == "me" && index == 2 {
            push("b", false);
        } else if verb == "mg" && index >= 2 {
            for candidate in MG_SAFE_FLAGS {
                if !before[2..].iter().any(|part| part == candidate) {
                    push(candidate, true);
                }
            }
        } else if !self.readonly
            && matches!(
                verb,
                "ms" | "set" | "add" | "replace" | "append" | "prepend" | "cas"
            )
            && index == 2
        {
            push("--file", true);
            push("--base64", true);
        } else if !self.readonly
            && matches!(verb, "ms" | "md" | "ma")
            && index >= (if verb == "ms" { value_end } else { 2 })
        {
            let flags_start = if verb == "ms" { value_end } else { 2 };
            let options = match verb {
                "ms" => MS_FLAGS,
                "md" => MD_FLAGS,
                _ => MA_FLAGS,
            };
            for candidate in options {
                if !before[flags_start..].iter().any(|part| part == candidate) {
                    push(candidate, true);
                }
            }
        } else if !self.readonly
            && matches!(verb, "set" | "add" | "replace" | "cas")
            && index >= value_end
        {
            for candidate in if verb == "cas" {
                &["--cas", "--ttl", "--flags"][..]
            } else {
                &["--ttl", "--flags"][..]
            } {
                if !before.iter().any(|part| part == candidate) {
                    push(candidate, true);
                }
            }
        } else if !self.readonly && verb == "touch" && index == 2 {
            push("--ttl", true);
        } else if !self.readonly && verb == "flush_all" && index == 1 {
            push("--delay", true);
        }
        CompletionResult::fresh(suggestions)
    }
}

pub struct ShellPrompt {
    pub label: String,
    pub warning: Arc<Mutex<Option<Instant>>>,
}

impl Prompt for ShellPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.label)
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        let deadline = *self.warning.lock();
        if deadline.is_some_and(|deadline| Instant::now() < deadline) {
            Cow::Borrowed("Press Ctrl-C again to exit")
        } else {
            Cow::Borrowed("")
        }
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("> ")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("... ")
    }

    fn render_prompt_history_search_indicator(&self, _search: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Borrowed("(reverse-search)> ")
    }

    fn get_prompt_color(&self) -> Color {
        Color::Cyan
    }
}

/// Return true only for a second idle Ctrl-C before the three-second deadline.
pub fn armed_exit(warning: &Arc<Mutex<Option<Instant>>>) -> bool {
    let mut deadline = warning.lock();
    let now = Instant::now();
    if deadline.is_some_and(|until| now < until) {
        *deadline = None;
        true
    } else {
        *deadline = Some(now + Duration::from_secs(3));
        false
    }
}

pub fn clear_warning(warning: &Arc<Mutex<Option<Instant>>>) {
    *warning.lock() = None;
}

fn expire_warning(warning: &Arc<Mutex<Option<Instant>>>, now: Instant) -> bool {
    let mut deadline = warning.lock();
    if deadline.is_some_and(|until| now >= until) {
        *deadline = None;
        true
    } else {
        false
    }
}

/// Default history is memory-only; the idle callback repaints just once when the warning expires.
pub fn editor(
    readonly: bool,
    color: bool,
    known_keys: Arc<Mutex<Vec<String>>>,
    warning: Arc<Mutex<Option<Instant>>>,
) -> Reedline {
    let menu = ColumnarMenu::default().with_name("completion_menu");
    let mut keys = default_emacs_keybindings();
    keys.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::HistoryHintComplete,
            ReedlineEvent::Menu("completion_menu".into()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keys.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".into()),
            ReedlineEvent::MenuPrevious,
        ]),
    );
    let mut line = Reedline::create()
        .with_ansi_colors(color)
        .with_hinter(Box::new(CommandHinter::new(readonly)))
        .with_highlighter(Box::new(CommandHighlighter { readonly }))
        .with_completer(Box::new(CommandCompleter {
            readonly,
            known_keys,
        }))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(Box::new(Emacs::new(keys)))
        .with_poll_interval(Duration::from_millis(50));
    let repaint = line.repaint_signal();
    line.with_idle_callback(Box::new(move || {
        if expire_warning(&warning, Instant::now()) {
            repaint.request_repaint();
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::FileBackedHistory;

    #[test]
    fn hint_is_stable_and_only_a_command_suffix() {
        let history = FileBackedHistory::default();
        let mut hinter = CommandHinter::new(false);
        assert_eq!(hinter.handle("g", 1, &history, false, ""), "et ");
        assert_eq!(hinter.complete_hint(), "et ");
        assert_eq!(format!("g{}", hinter.complete_hint()), "get ");
        assert_eq!(hinter.handle("get", 3, &history, false, ""), "");
        assert_eq!(hinter.handle("get ", 4, &history, false, ""), "");
        assert_eq!(hinter.complete_hint(), "");
        assert_eq!(hinter.handle("gets", 4, &history, false, ""), "");
        assert_eq!(hinter.handle("g", 0, &history, false, ""), "");
    }

    #[test]
    fn readonly_hints_and_completions_exclude_mutations() {
        let history = FileBackedHistory::default();
        let mut hinter = CommandHinter::new(true);
        assert_eq!(hinter.handle("s", 1, &history, false, ""), "tats ");
        assert_eq!(hinter.handle("set", 3, &history, false, ""), "");
        assert_eq!(hinter.handle("mg", 2, &history, false, ""), "");
        let mut completer = CommandCompleter {
            readonly: true,
            known_keys: Arc::default(),
        };
        let result = completer.complete("", 0);
        assert!(result.suggestions().iter().all(|s| !matches!(
            s.value.as_str(),
            "set" | "gat" | "flush_all" | "mg" | "inspect"
        )));
        assert!(completer.complete("mg key ", 7).suggestions().is_empty());
        assert!(completer.complete("recent ", 7).suggestions().is_empty());
    }

    #[test]
    fn completes_only_token_span_using_seen_keys() {
        let mut completer = CommandCompleter {
            readonly: false,
            known_keys: Arc::new(Mutex::new(vec!["user:42".into(), "user:77".into()])),
        };
        let result = completer.complete("get us other", 6);
        assert_eq!(result.suggestions().len(), 2);
        assert_eq!(result.suggestions()[0].value, "user:42");
        assert_eq!(result.suggestions()[0].span, Span::new(4, 6));
        assert_eq!(
            completer.complete("stats se", 8).suggestions()[0].value,
            "settings"
        );
        let quoted_value = "set user:42 \"hello world\" --fl";
        assert_eq!(
            completer
                .complete(quoted_value, quoted_value.len())
                .suggestions()[0]
                .value,
            "--flags"
        );
        assert_eq!(completer.complete("get", 3).suggestions()[0].value, "get");
    }

    #[test]
    fn pasted_multibyte_whitespace_does_not_crash_completion() {
        let mut completer = CommandCompleter {
            readonly: false,
            known_keys: Arc::new(Mutex::new(vec!["user:42".into()])),
        };
        let line = "get\u{a0}user";
        assert!(
            completer
                .complete(line, line.len())
                .suggestions()
                .is_empty()
        );
    }

    #[test]
    fn idle_ctrl_c_expires_and_clears_prompt() {
        let warning = Arc::new(Mutex::new(None));
        let prompt = ShellPrompt {
            label: "disconnected".into(),
            warning: warning.clone(),
        };
        assert!(!armed_exit(&warning));
        assert_eq!(prompt.render_prompt_right(), "Press Ctrl-C again to exit");
        assert!(armed_exit(&warning));
        assert_eq!(prompt.render_prompt_right(), "");
        assert!(!armed_exit(&warning));
        let deadline = *warning.lock();
        assert!(!expire_warning(
            &warning,
            deadline.unwrap() - Duration::from_millis(1)
        ));
        assert!(expire_warning(&warning, deadline.unwrap()));
        assert_eq!(prompt.render_prompt_right(), "");
        assert!(!armed_exit(&warning));
        clear_warning(&warning);
        assert_eq!(prompt.render_prompt_right(), "");
    }
}
