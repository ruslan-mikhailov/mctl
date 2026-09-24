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
    ColumnarMenu, DefaultCompleter, Emacs, KeyCode, KeyModifiers, MenuBuilder, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, default_emacs_keybindings,
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
                if !self.readonly {
                    if let Some(recent) = &mut self.recent {
                        if let Err(error) = recent.record(self.endpoint.clone()) {
                            eprintln!(
                                "warning: host will not be remembered: {}",
                                render::escape(&error.to_string())
                            );
                        }
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
                Signal::CtrlC => {
                    if terminal::armed_exit(&self.warning) {
                        break;
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
        let command = match command::parse(line) {
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
                    match editor
                        .read_line(&prompt)
                        .map_err(|error| error.to_string())?
                    {
                        Signal::Success(confirmation)
                            if confirmation.trim() == self.endpoint.address() => {}
                        _ => {
                            println!("Cancelled; no request sent.");
                            return Ok(true);
                        }
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
        "\n[ro] Only get, gets, version, safe stats, me, mn, help, history, recent, reconnect, quit, exit are allowed; mg/inspect can claim stale-item recache ownership."
    } else {
        ""
    };
    let body = match verb {
        None => {
            "get/gets KEY [KEY...]; set/add/replace KEY VALUE [--ttl SEC] [--flags N]; append/prepend KEY VALUE; cas KEY VALUE --cas ID; delete KEY; incr/decr KEY DELTA; touch KEY --ttl SEC; gat/gats SEC KEY [KEY...]; stats [items|slabs|settings|sizes]; version; flush_all [--delay SEC]; mg/ms/md/ma/me/mn; inspect KEY [--value]; help [command]; history; recent [forget HOST:PORT [--tls]|clear]; reconnect; quit/exit. VALUE may be quoted, --base64 TEXT, or --file PATH. TTL is relative, <=30 days."
        }
        Some("mg") => {
            "mg KEY [FLAGS...]: f/c/t/s/v request flags/CAS/TTL/size/value; h/l/k/u/b/Otoken inspect metadata; Nttl/Rthreshold/Tttl/Ecas mutate state; quiet q is unsupported."
        }
        Some("md") => {
            "md KEY [FLAGS...]: I marks stale; Tttl with I updates expiry; x removes only the value. Mutating operation."
        }
        Some("stats sizes") => {
            "stats sizes: on Memcached versions before 1.4.27 this can lock the server for minutes; newer versions avoid that particular lock."
        }
        Some(other) => {
            return format!("{other}: use help for command syntax; unknown help topic{suffix}");
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
            "recent clear",
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
}
