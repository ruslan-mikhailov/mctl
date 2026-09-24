use std::io::{self, BufRead, IsTerminal};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use mctl::command::{self, BasicCommand, Command, LocalCommand, MetaCommand, RecentCommand};
use mctl::connection::{self, ReadWrite};
use mctl::recent::{Endpoint, RecentStore};
use mctl::render;
use mctl::terminal::{self, ShellPrompt};
use mctl::wire::{Wire, WireResponse};
use parking_lot::Mutex;
use reedline::{
    ColumnarMenu, DefaultCompleter, EditCommand, Emacs, KeyCode, KeyModifiers, MenuBuilder,
    Reedline, ReedlineEvent, ReedlineMenu, Signal, default_emacs_keybindings,
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Parser)]
#[command(about = "Interactive Memcached text-protocol shell")]
struct Options {
    /// Host and optional port; omit to choose a recent host interactively.
    host: Option<String>,
    /// Use verified TLS without plaintext fallback.
    #[arg(long)]
    tls: bool,
    /// Reject cache mutations before sending any request.
    #[arg(long)]
    readonly: bool,
    /// Network command timeout in seconds.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..))]
    timeout: u64,
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto)]
    color: ColorChoice,
    /// Do not read or update the recent-host list.
    #[arg(long)]
    no_recent_hosts: bool,
}

type Network = Wire<Box<dyn ReadWrite + Send>>;

struct Session {
    endpoint: Endpoint,
    network: Option<Network>,
    recent: Option<RecentStore>,
    readonly: bool,
    color: bool,
    timeout: Duration,
    keys: Arc<Mutex<Vec<String>>>,
    history: Vec<String>,
    running: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    warning: Arc<Mutex<Option<Instant>>>,
}

struct RunningGuard {
    state: Arc<AtomicBool>,
    previous: bool,
}

impl RunningGuard {
    fn start(state: &Arc<AtomicBool>) -> Self {
        Self {
            state: state.clone(),
            previous: state.swap(true, Ordering::AcqRel),
        }
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.state.store(self.previous, Ordering::Release);
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {}", render::escape(&error));
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let options = Options::parse();
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let color = match options.color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            interactive
                && std::env::var_os("NO_COLOR").is_none()
                && std::env::var("TERM").is_ok_and(|term| term != "dumb")
        }
    };
    let recent = if options.no_recent_hosts {
        None
    } else {
        match RecentStore::default_path().and_then(RecentStore::load) {
            Ok(store) => Some(store),
            Err(error) => {
                eprintln!(
                    "warning: cannot load recent hosts: {}",
                    render::escape(&error.to_string())
                );
                None
            }
        }
    };
    let endpoint = if let Some(host) = &options.host {
        Endpoint::parse(host, options.tls)?
    } else if interactive {
        match pick_target(recent.as_ref(), options.tls)? {
            Some(endpoint) => endpoint,
            None => return Ok(()),
        }
    } else {
        return Err("HOST:PORT is required when stdin or stdout is not a terminal".into());
    };
    let running = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));
    let handler_running = running.clone();
    let handler_cancelled = cancelled.clone();
    ctrlc::set_handler(move || {
        if handler_running.load(Ordering::Acquire) {
            handler_cancelled.store(true, Ordering::Release);
        }
    })
    .map_err(|error| format!("cannot install Ctrl-C handler: {error}"))?;

    let mut session = Session {
        endpoint,
        network: None,
        recent,
        readonly: options.readonly,
        color,
        timeout: Duration::from_secs(options.timeout),
        keys: Arc::new(Mutex::new(Vec::new())),
        history: Vec::new(),
        running,
        cancelled,
        warning: Arc::new(Mutex::new(None)),
    };
    session.connect();
    if interactive {
        session.interactive()
    } else {
        session.batch()
    }
}

fn selected_recent(
    recent: Option<&RecentStore>,
    choice: &str,
    force_tls: bool,
) -> Option<Endpoint> {
    let index = choice.parse::<usize>().ok()?;
    let mut endpoint = recent?.entries().get(index.checked_sub(1)?)?.clone();
    endpoint.tls |= force_tls;
    Some(endpoint)
}

fn pick_target(recent: Option<&RecentStore>, force_tls: bool) -> Result<Option<Endpoint>, String> {
    let warning = Arc::new(Mutex::new(None));
    let suggestions = recent
        .map(|store| store.entries().iter().map(Endpoint::address).collect())
        .unwrap_or_else(|| vec!["127.0.0.1:11211".into()]);
    let mut keys = default_emacs_keybindings();
    keys.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("recent_hosts".into()),
            ReedlineEvent::MenuNext,
        ]),
    );
    let mut picker = Reedline::create()
        .with_completer(Box::new(DefaultCompleter::new_with_wordlen(suggestions, 1)))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default().with_name("recent_hosts"),
        )))
        .with_edit_mode(Box::new(Emacs::new(keys)));
    let prompt = ShellPrompt {
        label: "connect".into(),
        warning: warning.clone(),
    };
    println!("Connect to:");
    if let Some(store) = recent {
        for (index, endpoint) in store.entries().iter().enumerate() {
            println!(
                "  {}  {}   {}",
                index + 1,
                endpoint.address(),
                if endpoint.tls { "tls" } else { "plain" }
            );
        }
    }
    println!("  n  New host");
    if recent.is_none_or(|store| store.entries().is_empty()) {
        println!("  Enter  127.0.0.1:11211 (suggested; no connection until selected)");
    }
    let answer = match picker
        .read_line(&prompt)
        .map_err(|error| error.to_string())?
    {
        Signal::Success(answer) => answer,
        Signal::CtrlC | Signal::CtrlD => return Ok(None),
        _ => return Ok(None),
    };
    let choice = answer.trim();
    if let Some(endpoint) = selected_recent(recent, choice, force_tls) {
        return Ok(Some(endpoint));
    }
    if choice.parse::<usize>().is_ok() {
        return Err("invalid recent-host selection".into());
    }
    if choice.is_empty() && recent.is_none_or(|store| store.entries().is_empty()) {
        return Ok(Some(Endpoint::parse("127.0.0.1:11211", force_tls)?));
    }
    let host = if choice == "n" {
        let prompt = ShellPrompt {
            label: "host".into(),
            warning,
        };
        match picker
            .read_line(&prompt)
            .map_err(|error| error.to_string())?
        {
            Signal::Success(host) => host.trim().to_owned(),
            _ => return Ok(None),
        }
    } else {
        choice.to_owned()
    };
    if host.is_empty() {
        return Ok(None);
    }
    let tls = if force_tls {
        true
    } else {
        let prompt = ShellPrompt {
            label: "TLS? [y/N]".into(),
            warning: Arc::new(Mutex::new(None)),
        };
        match picker
            .read_line(&prompt)
            .map_err(|error| error.to_string())?
        {
            Signal::Success(answer) => match answer.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => true,
                "" | "n" | "no" => false,
                _ => return Err("answer y or n for TLS".into()),
            },
            _ => return Ok(None),
        }
    };
    Endpoint::parse(&host, tls).map(Some)
}

fn flush_confirmed(editor: &mut Reedline, signal: Signal, address: &str) -> bool {
    match signal {
        Signal::Success(confirmation) if confirmation.trim() == address => true,
        _ => {
            editor.run_edit_commands(&[EditCommand::Clear]);
            false
        }
    }
}

impl Session {
    fn connect(&mut self) {
        self.cancelled.store(false, Ordering::Release);
        let _running = RunningGuard::start(&self.running);
        let result = connection::connect(&self.endpoint, self.timeout, &self.cancelled);
        self.network = match result {
            Ok(connected) => {
                println!(
                    "Connected to {} ({}). Type help.",
                    self.endpoint.address(),
                    if self.endpoint.tls {
                        "tls"
                    } else {
                        "text protocol"
                    }
                );
                if let Some(recent) = &mut self.recent {
                    if let Err(error) = recent.record(self.endpoint.clone()) {
                        eprintln!(
                            "warning: host will not be remembered: {}",
                            render::escape(&error.to_string())
                        );
                    }
                }
                Some(Wire::with_deadline(
                    connected.stream,
                    self.timeout,
                    connected.deadline,
                ))
            }
            Err(error) => {
                eprintln!(
                    "{}",
                    render::error(
                        &format!("connection to {} failed: {error}", self.endpoint.address()),
                        self.color
                    )
                );
                None
            }
        };
    }

    fn interactive(&mut self) -> Result<(), String> {
        let mut editor = terminal::editor(
            self.readonly,
            self.color,
            self.keys.clone(),
            self.warning.clone(),
        );
        loop {
            let label = self.prompt_label();
            let prompt = ShellPrompt {
                label,
                warning: self.warning.clone(),
            };
            match editor
                .read_line(&prompt)
                .map_err(|error| error.to_string())?
            {
                Signal::Success(line) => {
                    if !line.trim().is_empty() && !self.handle(&line, Some(&mut editor))? {
                        break;
                    }
                }
                Signal::HostCommand(command) if command == terminal::INTERRUPT_COMMAND => {
                    let input = editor.current_buffer_contents();
                    if terminal::armed_exit(&self.warning, input) {
                        break;
                    }
                    if !input.is_empty() {
                        editor.run_edit_commands(&[EditCommand::Clear]);
                        println!();
                    }
                }
                Signal::CtrlD => break,
                _ => {}
            }
        }
        Ok(())
    }

    fn batch(&mut self) -> Result<(), String> {
        for line in io::stdin().lock().lines() {
            let line = line.map_err(|error| error.to_string())?;
            if !line.trim().is_empty() && !self.handle(&line, None)? {
                break;
            }
        }
        Ok(())
    }

    fn prompt_label(&self) -> String {
        let address = if self.network.is_some() {
            self.endpoint.address()
        } else {
            "disconnected".into()
        };
        let tls = if self.endpoint.tls && self.network.is_some() {
            " [tls]"
        } else {
            ""
        };
        let ro = if self.readonly { " [ro]" } else { "" };
        format!("{address}{tls}{ro}")
    }

    fn handle(&mut self, line: &str, mut editor: Option<&mut Reedline>) -> Result<bool, String> {
        terminal::clear_warning(&self.warning);
        self.cancelled.store(false, Ordering::Release);
        let _running = RunningGuard::start(&self.running);
        self.history.push(line.to_owned());
        let command = match command::parse_for_session(line, self.readonly) {
            Ok(command) => command,
            Err(error) => {
                println!("{}", render::error(&error, self.color));
                return Ok(true);
            }
        };
        if self.cancelled.load(Ordering::Acquire) {
            println!("Cancelled; no request sent.");
            return Ok(true);
        }
        if self.readonly {
            if let Some(error) = command.readonly_error() {
                println!("{}", render::error(error, self.color));
                return Ok(true);
            }
        }
        match &command {
            Command::Local(local) => return self.local(local, editor.as_deref_mut()),
            Command::Basic(BasicCommand::FlushAll { .. }) => {
                if let Some(editor) = editor.as_deref_mut() {
                    let prompt = ShellPrompt {
                        label: format!("Type {} to confirm", self.endpoint.address()),
                        warning: Arc::new(Mutex::new(None)),
                    };
                    println!("Flush all keys on {}?", self.endpoint.address());
                    let confirmation = editor
                        .read_line(&prompt)
                        .map_err(|error| error.to_string())?;
                    if !flush_confirmed(editor, confirmation, &self.endpoint.address()) {
                        println!("Cancelled; no request sent.");
                        return Ok(true);
                    }
                } else {
                    println!(
                        "{}",
                        render::error(
                            "flush_all requires an interactive confirmation; no request sent",
                            self.color
                        )
                    );
                    return Ok(true);
                }
            }
            _ => {}
        }
        let request = match &command {
            Command::Basic(basic) => mctl::request::basic(basic),
            Command::Meta(meta) => mctl::request::meta(meta),
            Command::Local(_) => unreachable!(),
        };
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                println!("{}", render::error(&error, self.color));
                return Ok(true);
            }
        };
        let Some(network) = &mut self.network else {
            println!(
                "{}",
                render::error("disconnected; use reconnect", self.color)
            );
            return Ok(true);
        };
        if self.cancelled.load(Ordering::Acquire) {
            println!("Cancelled; no request sent.");
            return Ok(true);
        }
        let result = network.execute(&request.bytes, request.response, &self.cancelled);
        match result {
            Ok(response) => {
                println!("{}", render::response(&command, &response, self.color));
                self.remember_keys(&command, &response);
            }
            Err(error) => {
                self.network = None;
                if error.kind() == io::ErrorKind::Interrupted
                    && self.cancelled.load(Ordering::Acquire)
                {
                    println!("Cancelled; outcome may be unknown. Disconnected; use reconnect.");
                } else {
                    println!(
                        "{}",
                        render::error(&format!("{error}; disconnected; use reconnect"), self.color)
                    );
                }
            }
        }
        Ok(true)
    }

    fn local(
        &mut self,
        command: &LocalCommand,
        _editor: Option<&mut Reedline>,
    ) -> Result<bool, String> {
        match command {
            LocalCommand::Quit | LocalCommand::Exit => return Ok(false),
            LocalCommand::Help(verb) => println!("{}", help(verb.as_deref(), self.readonly)),
            LocalCommand::History => {
                for (index, line) in self.history.iter().enumerate() {
                    println!("{:>4}  {}", index + 1, render::escape(line));
                }
            }
            LocalCommand::Recent(action) => {
                if let Some(recent) = &mut self.recent {
                    match action {
                        RecentCommand::List => {
                            for (index, endpoint) in recent.entries().iter().enumerate() {
                                println!(
                                    "{}  {}  {}",
                                    index + 1,
                                    endpoint.address(),
                                    if endpoint.tls { "tls" } else { "plain" }
                                );
                            }
                        }
                        RecentCommand::Forget { address, tls } => {
                            match recent.forget(address, *tls) {
                                Ok(count) => println!("Forgot {count} endpoint(s)."),
                                Err(error) => {
                                    println!("{}", render::error(&error.to_string(), self.color))
                                }
                            }
                        }
                        RecentCommand::Clear => {
                            if let Err(error) = recent.clear() {
                                println!("{}", render::error(&error.to_string(), self.color));
                            }
                        }
                    }
                } else {
                    println!("recent hosts unavailable or disabled");
                }
            }
            LocalCommand::Reconnect => {
                self.network = None;
                self.connect();
            }
        }
        Ok(true)
    }

    fn remember_keys(&mut self, command: &Command, result: &WireResponse) {
        let mut keys = self.keys.lock();
        match result {
            WireResponse::Values(items) => {
                for item in items {
                    if let Ok(key) = std::str::from_utf8(&item.key) {
                        if !keys.iter().any(|stored| stored == key) {
                            keys.push(key.to_owned());
                        }
                    }
                }
            }
            WireResponse::Status(status) if status == "STORED" => {
                if let Command::Basic(BasicCommand::Store { key, .. }) = command {
                    if !keys.contains(key) {
                        keys.push(key.clone());
                    }
                }
            }
            WireResponse::Meta { code, .. } if code == "HD" => {
                if let Command::Meta(MetaCommand::Set { key, .. }) = command {
                    if !keys.contains(key) {
                        keys.push(key.clone());
                    }
                }
            }
            _ => {}
        }
        let excess = keys.len().saturating_sub(1000);
        keys.drain(..excess);
    }
}

fn help(verb: Option<&str>, readonly: bool) -> String {
    let suffix = if readonly {
        "\n[ro] Remote commands allowed: get, gets, stats, version, me, mn. mg/inspect can claim stale-item recache ownership; remote mutations are blocked. Local help, history, recent, reconnect, quit, and exit remain available."
    } else {
        ""
    };
    let body = match verb {
        None => {
            "Commands (help COMMAND for details):
  get KEY [KEY...]                     Retrieve values.
  gets KEY [KEY...]                    Retrieve values with CAS IDs.
  set KEY VALUE [TTL [FLAGS]]          Store a value.
  add KEY VALUE [TTL [FLAGS]]          Store only if absent.
  replace KEY VALUE [TTL [FLAGS]]      Store only if present.
  append KEY VALUE                    Append to an existing value.
  prepend KEY VALUE                   Prepend to an existing value.
  cas KEY VALUE CAS_ID [TTL [FLAGS]]   Store if the CAS ID matches.
  delete KEY                          Delete a key.
  incr KEY DELTA                      Increase an unsigned counter.
  decr KEY DELTA                      Decrease an unsigned counter.
  touch KEY TTL                       Update expiration.
  gat SEC KEY [KEY...]                Update expiration and retrieve values.
  gats SEC KEY [KEY...]               As gat, with CAS IDs.
  stats [items|slabs|settings|sizes]  Read server statistics; see help stats.
  version                             Show server version.
  flush_all [DELAY]                   Invalidate all keys; interactive confirmation required.
  mg KEY [FLAGS...]                   Meta get.
  ms KEY VALUE [FLAGS...]             Meta set.
  md KEY [FLAGS...]                   Meta delete.
  ma KEY [FLAGS...]                   Meta arithmetic.
  me KEY [b]                          Meta debug.
  mn                                  Meta no-op.
  inspect KEY [value]                 Show item metadata (and optionally value).
  help [COMMAND]                      Show command help.
  history                             Show entered commands.
  recent [forget HOST:PORT [tls]|clear]  List or change recent hosts.
  reconnect                           Reconnect to the current host.
  quit                                Leave the shell.
  exit                                Leave the shell.
VALUE may be inline (quote whitespace), --base64 TEXT, or --file PATH.
TTL and DELAY are relative seconds (0..=30 days).
Named --ttl, --flags, --cas, and --delay accept either --name VALUE or --name=VALUE; --base64 and --file do too. --value and --tls are switches.
get/gets only retrieve; they do not accept VALUE, TTL, or FLAGS."
        }
        Some("get") => {
            "get KEY [KEY...]: retrieve one or more values. Retrieval only; no VALUE, TTL, or FLAGS."
        }
        Some("gets") => {
            "gets KEY [KEY...]: retrieve one or more values with their CAS IDs. Retrieval only; no VALUE, TTL, or FLAGS."
        }
        Some("set" | "add" | "replace") => {
            let verb = verb.unwrap();
            let effect = match verb {
                "set" => "store the value",
                "add" => "store only if the key does not exist",
                _ => "store only if the key exists",
            };
            return format!(
                "{verb} KEY VALUE [TTL [FLAGS]]\n\
                 {verb} KEY --base64 ENCODED_VALUE [TTL [FLAGS]]\n\
                 {verb} KEY --file PATH [TTL [FLAGS]]\n\
                 {verb} KEY VALUE [--ttl SEC] [--flags N]\n\
                 {verb} KEY --base64 ENCODED_VALUE [--ttl SEC] [--flags N]\n\
                 {verb} KEY --file PATH [--ttl SEC] [--flags N]\n\
                 {effect}. Inline VALUE can be quoted to preserve whitespace. \
                 TTL is relative seconds (0..=30 days); FLAGS is an unsigned 32-bit integer. \
                 Do not mix positional TTL/FLAGS with named options. Valued options also accept --name=value (for example, --flags=123 and --file=PATH).{suffix}"
            );
        }
        Some("append") => {
            "append KEY VALUE: append to an existing value. VALUE may be inline, --base64 TEXT, or --file PATH. No TTL or FLAGS options."
        }
        Some("prepend") => {
            "prepend KEY VALUE: prepend to an existing value. VALUE may be inline, --base64 TEXT, or --file PATH. No TTL or FLAGS options."
        }
        Some("cas") => {
            "cas KEY VALUE CAS_ID [TTL [FLAGS]]\ncas KEY VALUE --cas ID [--ttl SEC] [--flags N]\nStore only when the CAS ID matches. VALUE may be inline, --base64 TEXT, or --file PATH. CAS_ID is an unsigned 64-bit integer; TTL is relative seconds (0..=30 days); FLAGS is an unsigned 32-bit integer. Do not mix positional numbers with named options."
        }
        Some("delete") => "delete KEY: delete a cached item.",
        Some("incr") => {
            "incr KEY DELTA: increase an unsigned decimal counter by an unsigned 64-bit DELTA."
        }
        Some("decr") => {
            "decr KEY DELTA: decrease an unsigned decimal counter by an unsigned 64-bit DELTA."
        }
        Some("touch") => {
            "touch KEY TTL\ntouch KEY --ttl SEC\nUpdate a key's expiration. TTL is relative seconds (0..=30 days)."
        }
        Some("gat") => {
            "gat SEC KEY [KEY...]: update expiration and retrieve values. SEC is relative seconds (0..=30 days); this mutates expiration."
        }
        Some("gats") => {
            "gats SEC KEY [KEY...]: update expiration and retrieve values with CAS IDs. SEC is relative seconds (0..=30 days); this mutates expiration."
        }
        Some("stats") => {
            "stats [items|slabs|settings|sizes]: read general, item, slab, configuration, or size statistics. stats sizes can lock Memcached versions before 1.4.27 for minutes; see help stats sizes."
        }
        Some("stats items") => "stats items: show statistics grouped by slab class.",
        Some("stats slabs") => "stats slabs: show slab allocation statistics.",
        Some("stats settings") => "stats settings: show server configuration settings.",
        Some("stats sizes") => {
            "stats sizes: on Memcached versions before 1.4.27 this can lock the server for minutes; newer versions avoid that particular lock."
        }
        Some("version") => "version: show the Memcached server version.",
        Some("flush_all") => {
            "flush_all [DELAY]\nflush_all [--delay SEC]\nInvalidate all cached items, immediately or after relative DELAY seconds (0..=30 days). Requires interactive confirmation by typing the host address; batch mode refuses it."
        }
        Some("mg") => {
            "mg KEY [FLAGS...]: meta get. f/c/t/s/v request client flags/CAS/remaining TTL/size/value; h/l/k/u/b/O<TOKEN> request hit, last access, key, no LRU bump, binary key, or opaque token. N<TTL>/R<TTL>/T<TTL>/E<CAS> can mutate state; q is unsupported. Blocked in readonly mode."
        }
        Some("ms") => {
            "ms KEY VALUE [FLAGS...]: meta set. VALUE may be inline, --base64 TEXT, or --file PATH. Supported flags include T<TTL>, F<N>, C<CAS>, E<CAS>, I (requires C<CAS>), M[E|R|A|P|S], N<TTL> (requires MA), b, c, k, O<TOKEN>, s. q is unsupported."
        }
        Some("md") => {
            "md KEY [FLAGS...]: meta delete. I marks stale; T<TTL> with I updates expiration; x removes only the value. Also supports C<CAS>, E<CAS>, b, k, O<TOKEN>. Mutating operation; q is unsupported."
        }
        Some("ma") => {
            "ma KEY [FLAGS...]: meta arithmetic. D<N> is the delta; M[I|+|D|-] selects arithmetic mode; N<TTL> can create an item (J<N> requires N). Also supports C<CAS>, E<CAS>, T<TTL>, b, c, k, O<TOKEN>, t, v. q is unsupported."
        }
        Some("me") => {
            "me KEY [b]: inspect the server's meta debug information; b denotes a base64-encoded binary key."
        }
        Some("mn") => "mn: send a meta no-op to check server responsiveness.",
        Some("inspect") => {
            "inspect KEY [value]\ninspect KEY [--value]\nShow item flags, CAS, remaining TTL, size, hit, and last-access metadata. Add value/--value to include the value. Blocked in readonly mode because meta get can claim recache ownership."
        }
        Some("help") => {
            "help [COMMAND]: show this overview or help for one command (for example, help set or help stats sizes)."
        }
        Some("history") => "history: list commands entered during this session.",
        Some("recent") => {
            "recent: list saved hosts.\nrecent forget HOST:PORT [tls]\nrecent forget HOST:PORT [--tls]\nForget saved entries for that address; tls/--tls restricts removal to TLS entries. recent clear removes all saved hosts. Local recent-host changes are available in readonly mode."
        }
        Some("recent forget") => {
            "recent forget HOST:PORT [tls]\nrecent forget HOST:PORT [--tls]\nForget saved hosts at this address; tls/--tls limits removal to TLS entries. Available in readonly mode."
        }
        Some("recent clear") => {
            "recent clear: remove every saved host. Available in readonly mode."
        }
        Some("reconnect") => "reconnect: reconnect to the current host.",
        Some("quit") => "quit: leave the shell.",
        Some("exit") => "exit: leave the shell.",
        Some(other) => {
            return format!(
                "unknown help topic {other}; use help to list supported commands{suffix}"
            );
        }
    };
    format!("{body}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Write};

    struct FakeNetwork {
        response: Cursor<Vec<u8>>,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl Read for FakeNetwork {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.response.read(bytes)
        }
    }

    impl Write for FakeNetwork {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.written.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn help_overview_lists_every_command_on_its_own_line_with_a_topic() {
        let overview = help(None, false);
        for verb in [
            "get",
            "gets",
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
            "stats",
            "version",
            "flush_all",
            "mg",
            "ms",
            "md",
            "ma",
            "me",
            "mn",
            "inspect",
            "help",
            "history",
            "recent",
            "reconnect",
            "quit",
            "exit",
        ] {
            assert!(
                overview
                    .lines()
                    .any(|line| line.trim_start().starts_with(&format!("{verb} "))),
                "overview omits {verb}"
            );
            let topic = help(Some(verb), false);
            assert!(
                !topic.contains("unknown help topic"),
                "missing topic for {verb}"
            );
            assert!(topic.starts_with(verb), "topic does not identify {verb}");
        }
        for topic in [
            "stats items",
            "stats slabs",
            "stats settings",
            "stats sizes",
            "recent forget",
            "recent clear",
        ] {
            assert!(
                !help(Some(topic), false).contains("unknown help topic"),
                "{topic}"
            );
        }
        assert!(help(Some("not-a-command"), false).contains("unknown help topic"));
    }

    #[test]
    fn help_explains_storage_forms_without_confusing_retrieval() {
        let set = help(Some("set"), false);
        for form in [
            "set KEY VALUE [TTL [FLAGS]]",
            "set KEY VALUE [--ttl SEC] [--flags N]",
            "set KEY --base64 ENCODED_VALUE [TTL [FLAGS]]",
            "set KEY --file PATH [TTL [FLAGS]]",
            "set KEY --base64 ENCODED_VALUE [--ttl SEC] [--flags N]",
            "set KEY --file PATH [--ttl SEC] [--flags N]",
        ] {
            assert!(set.contains(form), "set help omits {form}");
        }
        assert!(set.contains("Do not mix"));
        assert!(help(Some("cas"), false).contains("cas KEY VALUE CAS_ID [TTL [FLAGS]]"));
        assert!(help(Some("cas"), false).contains("--cas ID"));
        assert!(help(Some("touch"), false).contains("touch KEY TTL"));
        assert!(help(Some("flush_all"), false).contains("flush_all [DELAY]"));
        assert!(help(Some("inspect"), false).contains("inspect KEY [value]"));
        assert!(help(Some("recent forget"), false).contains("HOST:PORT [tls]"));
        for verb in ["get", "gets"] {
            let topic = help(Some(verb), false);
            assert!(topic.contains("KEY [KEY...]"));
            assert!(topic.contains("Retrieval only"));
        }
        for verb in ["append", "prepend"] {
            assert!(help(Some(verb), false).contains("No TTL or FLAGS"));
        }
    }

    #[test]
    fn help_keeps_destructive_operation_warnings_and_readonly_limits() {
        assert!(help(Some("stats"), false).contains("1.4.27"));
        let sizes = help(Some("stats sizes"), false);
        assert!(sizes.contains("before 1.4.27"));
        assert!(sizes.contains("lock the server for minutes"));
        let flush = help(Some("flush_all"), false);
        assert!(flush.contains("interactive confirmation"));
        assert!(flush.contains("batch mode refuses"));
        let readonly = help(None, true);
        assert!(readonly.contains("Local help, history, recent"));
        assert!(readonly.contains("mg/inspect"));
    }

    #[test]
    fn readonly_blocks_mutations_before_network_write_but_allows_reads() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let stream: Box<dyn ReadWrite + Send> = Box::new(FakeNetwork {
            response: Cursor::new(b"END\r\n".to_vec()),
            written: written.clone(),
        });
        let mut session = Session {
            endpoint: Endpoint::parse("localhost", false).unwrap(),
            network: Some(Wire::new(stream, Duration::from_secs(1))),
            recent: None,
            readonly: true,
            color: false,
            timeout: Duration::from_secs(1),
            keys: Arc::new(Mutex::new(Vec::new())),
            history: Vec::new(),
            running: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            warning: Arc::new(Mutex::new(None)),
        };
        for command in [
            "mg k",
            "mg k u",
            "inspect k",
            "set k v",
            "gat 30 k",
            "mg k T30",
            "mg k N30",
            "mg k R30",
            "md k",
            "ma k D2",
            "stats reset",
        ] {
            session.handle(command, None).unwrap();
        }
        assert!(
            written.lock().is_empty(),
            "blocked commands must not reach the server"
        );
        session.handle("get k", None).unwrap();
        assert_eq!(&*written.lock(), b"get k\r\n");
    }
    #[test]
    fn cancelled_flush_confirmation_discards_pending_input() {
        let mut editor = terminal::editor(
            false,
            false,
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(None)),
        );
        editor.run_edit_commands(&[EditCommand::InsertString("delete important".into())]);
        assert!(!flush_confirmed(
            &mut editor,
            Signal::HostCommand(terminal::INTERRUPT_COMMAND.into()),
            "cache:11211",
        ));
        assert_eq!(editor.current_buffer_contents(), "");
    }

    #[test]
    fn zero_is_not_a_recent_host_selection() {
        let dir = tempfile::tempdir().unwrap();
        let mut recent = RecentStore::load(dir.path().join("recent.json")).unwrap();
        recent
            .record(Endpoint::parse("cache.example", false).unwrap())
            .unwrap();
        assert_eq!(selected_recent(Some(&recent), "0", false), None);
        assert_eq!(
            selected_recent(Some(&recent), "1", false)
                .unwrap()
                .address(),
            "cache.example:11211"
        );
    }
    #[test]
    fn readonly_keeps_local_history_and_persists_recent_host_changes() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = Endpoint::parse(&listener.local_addr().unwrap().to_string(), false).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recent.json");
        let mut session = Session {
            endpoint: endpoint.clone(),
            network: None,
            recent: Some(RecentStore::load(path.clone()).unwrap()),
            readonly: true,
            color: false,
            timeout: Duration::from_secs(1),
            keys: Arc::new(Mutex::new(Vec::new())),
            history: Vec::new(),
            running: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            warning: Arc::new(Mutex::new(None)),
        };
        session.connect();
        assert_eq!(
            RecentStore::load(path.clone()).unwrap().entries(),
            &[endpoint.clone()]
        );

        let forget = format!("recent forget {}", endpoint.address());
        session.handle(&forget, None).unwrap();
        assert!(
            RecentStore::load(path.clone())
                .unwrap()
                .entries()
                .is_empty()
        );
        session.recent.as_mut().unwrap().record(endpoint).unwrap();
        session.handle("recent clear", None).unwrap();
        assert!(RecentStore::load(path).unwrap().entries().is_empty());

        session.handle("set test", None).unwrap();
        session.handle("history", None).unwrap();
        assert_eq!(
            session.history,
            [
                forget,
                "recent clear".into(),
                "set test".into(),
                "history".into()
            ]
        );
        drop(session);
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        socket.read_to_end(&mut request).unwrap();
        assert!(
            request.is_empty(),
            "local changes and blocked writes must send no bytes"
        );
    }
}
