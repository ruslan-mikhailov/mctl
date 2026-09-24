use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
#[cfg(unix)]
use std::fs::File;
use std::io::{self, Write};
use std::net::Ipv6Addr;
use std::path::PathBuf;

const MAX_RECENT: usize = 20;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

impl Endpoint {
    pub fn parse(input: &str, tls: bool) -> Result<Self, String> {
        let (host, port) = if let Some(rest) = input.strip_prefix('[') {
            let (address, suffix) = rest
                .split_once(']')
                .ok_or("IPv6 addresses must be enclosed in brackets")?;
            let ip: Ipv6Addr = address.parse().map_err(|_| "invalid IPv6 address")?;
            let port = suffix.strip_prefix(':').unwrap_or_default();
            if !suffix.is_empty() && !suffix.starts_with(':') {
                return Err("invalid address suffix".into());
            }
            (ip.to_string(), port)
        } else {
            let (name, port) = input.split_once(':').unwrap_or((input, ""));
            if port.contains(':') {
                return Err("IPv6 addresses must be enclosed in brackets".into());
            }
            (name.to_ascii_lowercase(), port)
        };
        if host.is_empty()
            || host.len() > 253
            || host
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control() || b == b'/' || b == b'@')
        {
            return Err("invalid host".into());
        }
        let port = if port.is_empty() {
            if input.ends_with(':') {
                return Err("port is missing".into());
            }
            11211
        } else {
            port.parse::<u16>().map_err(|_| "invalid port")?
        };
        if port == 0 {
            return Err("port must be nonzero".into());
        }
        Ok(Self { host, port, tls })
    }

    pub fn address(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Saved {
    version: u8,
    entries: Vec<Endpoint>,
}

#[derive(Serialize)]
struct SavedRef<'a> {
    version: u8,
    entries: &'a [Endpoint],
}

pub struct RecentStore {
    path: PathBuf,
    entries: Vec<Endpoint>,
}

impl RecentStore {
    pub fn default_path() -> io::Result<PathBuf> {
        ProjectDirs::from("", "", "mctl")
            .map(|dirs| dirs.data_local_dir().join("recent.json"))
            .ok_or_else(|| io::Error::other("cannot find application data directory"))
    }

    pub fn load(path: PathBuf) -> io::Result<Self> {
        let entries = match fs::read(&path) {
            Ok(bytes) => {
                let saved: Saved = serde_json::from_slice(&bytes).map_err(invalid_data)?;
                if saved.version != 1 || saved.entries.len() > MAX_RECENT {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid recent-host version or count",
                    ));
                }
                for entry in &saved.entries {
                    if Endpoint::parse(&entry.address(), entry.tls).ok().as_ref() != Some(entry) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid recent-host entry",
                        ));
                    }
                }
                saved.entries
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err),
        };
        Ok(Self { path, entries })
    }

    pub fn entries(&self) -> &[Endpoint] {
        &self.entries
    }

    pub fn record(&mut self, endpoint: Endpoint) -> io::Result<()> {
        self.entries.retain(|entry| entry != &endpoint);
        self.entries.insert(0, endpoint);
        self.entries.truncate(MAX_RECENT);
        self.save()
    }

    pub fn forget(&mut self, address: &str, tls: Option<bool>) -> io::Result<usize> {
        let target = Endpoint::parse(address, false).map_err(invalid_data)?;
        let before = self.entries.len();
        self.entries.retain(|entry| {
            entry.host != target.host
                || entry.port != target.port
                || tls.is_some_and(|mode| mode != entry.tls)
        });
        let removed = before - self.entries.len();
        if removed > 0 {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn clear(&mut self) -> io::Result<()> {
        self.entries.clear();
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("invalid recent-host path"))?;
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(
            &mut temp,
            &SavedRef {
                version: 1,
                entries: &self.entries,
            },
        )
        .map_err(invalid_data)?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path).map_err(|err| err.error)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_addresses_and_rejects_invalid_ports() {
        assert_eq!(
            Endpoint::parse("EXAMPLE.COM", false).unwrap().address(),
            "example.com:11211"
        );
        assert_eq!(
            Endpoint::parse("[::1]:11211", true).unwrap().address(),
            "[::1]:11211"
        );
        assert!(Endpoint::parse("::1", false).is_err());
        assert!(Endpoint::parse("host:0", false).is_err());
        assert!(Endpoint::parse("user@host", false).is_err());
    }

    #[test]
    fn persists_mru_with_distinct_tls_modes_and_forget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recent.json");
        let mut store = RecentStore::load(path.clone()).unwrap();
        store
            .record(Endpoint::parse("cache", false).unwrap())
            .unwrap();
        store
            .record(Endpoint::parse("CACHE:11211", true).unwrap())
            .unwrap();
        store
            .record(Endpoint::parse("cache", false).unwrap())
            .unwrap();
        assert_eq!(store.entries().len(), 2);
        assert!(!store.entries()[0].tls);
        assert_eq!(
            RecentStore::load(path.clone()).unwrap().entries(),
            store.entries()
        );
        assert_eq!(store.forget("cache", Some(true)).unwrap(), 1);
        assert_eq!(RecentStore::load(path).unwrap().entries().len(), 1);
    }

    #[test]
    fn malformed_data_is_not_silently_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recent.json");
        fs::write(&path, b"not json").unwrap();
        assert_eq!(
            RecentStore::load(path.clone()).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(path).unwrap(), b"not json");
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recent.json");
        let mut store = RecentStore::load(path.clone()).unwrap();
        store
            .record(Endpoint::parse("localhost", false).unwrap())
            .unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
