use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Agent runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    /// Address to listen on. Never defaults to a wildcard address.
    pub bind: SocketAddr,
    /// File holding the shared authentication token.
    pub token_path: PathBuf,
    /// TLS certificate file (created on first run if absent).
    pub cert_path: PathBuf,
    /// TLS private key file (created on first run if absent).
    pub key_path: PathBuf,
    /// Frames per second the agent aims to capture per window.
    pub target_fps: u16,
    /// Maximum unacknowledged frames in flight per window before the agent drops stale ones.
    pub max_frames_in_flight: u8,
    /// Permit `bind` to be a wildcard address.
    ///
    /// Refusing a wildcard by default is right for a normal host: this agent shares screen
    /// content and injects input, so listening on every interface must never happen by
    /// accident. But inside a VM whose only reachable path is one explicitly forwarded host
    /// port, the guest's own address is assigned by DHCP and is not known when the config is
    /// written — there, a wildcard is both necessary and no less safe than the forward itself.
    /// That case must be stated deliberately rather than inferred.
    pub allow_wildcard_bind: bool,
}

/// Error returned when configuration cannot be loaded.
#[derive(Debug)]
pub enum ConfigError {
    /// The file could not be read.
    Io(std::io::Error),
    /// A line was not `key = value`.
    Syntax { line: usize },
    /// A value could not be parsed as the expected type.
    Value { key: &'static str, line: usize },
    /// A required key was missing.
    Missing { key: &'static str },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(err) => write!(f, "could not read config file: {err}"),
            ConfigError::Syntax { line } => write!(f, "syntax error on line {line}"),
            ConfigError::Value { key, line } => {
                write!(f, "invalid value for `{key}` on line {line}")
            }
            ConfigError::Missing { key } => write!(f, "missing required key `{key}`"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(err) => Some(err),
            ConfigError::Syntax { .. }
            | ConfigError::Value { .. }
            | ConfigError::Missing { .. } => None,
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(err: std::io::Error) -> Self {
        ConfigError::Io(err)
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:7644"
                .parse()
                .expect("hardcoded valid SocketAddr literal"),
            token_path: PathBuf::from("oxagent-token.txt"),
            cert_path: PathBuf::from("oxagent-cert.pem"),
            key_path: PathBuf::from("oxagent-key.pem"),
            target_fps: 30,
            max_frames_in_flight: 2,
            allow_wildcard_bind: false,
        }
    }
}

/// A path value that must not be empty — an empty one would otherwise surface much later as an
/// opaque "file not found" instead of a config error pointing at the offending line.
fn non_empty_path(value: &str, key: &'static str, line: usize) -> Result<PathBuf, ConfigError> {
    if value.is_empty() {
        return Err(ConfigError::Value { key, line });
    }
    Ok(PathBuf::from(value))
}

impl AgentConfig {
    /// Parse configuration from the text of a config file.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut cfg = AgentConfig::default();
        let mut token_path_seen = false;
        let mut bind_line: Option<usize> = None;

        // Windows editors routinely prepend a UTF-8 BOM. It is not Unicode whitespace, so
        // without this the first key silently becomes an unknown key and its line is ignored.
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);

        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = idx + 1;
            let line = raw_line.trim();

            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let Some((key, value)) = line.split_once('=') else {
                return Err(ConfigError::Syntax { line: line_no });
            };

            let key = key.trim();
            let value = value.trim();

            match key {
                // A wildcard bind is refused unless `allow_wildcard_bind` says otherwise:
                // this agent shares screen content and injects input, so exposing it on every
                // interface must be a deliberate act, never a typo. The check happens after
                // the whole file is read so the two keys may appear in either order.
                "bind" => match value.parse::<SocketAddr>() {
                    Ok(addr) => {
                        cfg.bind = addr;
                        bind_line = Some(line_no);
                    }
                    Err(_) => {
                        return Err(ConfigError::Value {
                            key: "bind",
                            line: line_no,
                        })
                    }
                },
                "allow_wildcard_bind" => match value {
                    "true" => cfg.allow_wildcard_bind = true,
                    "false" => cfg.allow_wildcard_bind = false,
                    _ => {
                        return Err(ConfigError::Value {
                            key: "allow_wildcard_bind",
                            line: line_no,
                        })
                    }
                },
                "token_path" => {
                    cfg.token_path = non_empty_path(value, "token_path", line_no)?;
                    token_path_seen = true;
                }
                "cert_path" => cfg.cert_path = non_empty_path(value, "cert_path", line_no)?,
                "key_path" => cfg.key_path = non_empty_path(value, "key_path", line_no)?,
                "target_fps" => match value.parse::<u16>() {
                    Ok(fps) if (1..=240).contains(&fps) => cfg.target_fps = fps,
                    _ => {
                        return Err(ConfigError::Value {
                            key: "target_fps",
                            line: line_no,
                        })
                    }
                },
                "max_frames_in_flight" => match value.parse::<u8>() {
                    Ok(m) if (1..=16).contains(&m) => cfg.max_frames_in_flight = m,
                    _ => {
                        return Err(ConfigError::Value {
                            key: "max_frames_in_flight",
                            line: line_no,
                        })
                    }
                },
                _ => {}
            }
        }

        if !token_path_seen {
            return Err(ConfigError::Missing { key: "token_path" });
        }
        // Checked here rather than at the `bind` line so the two keys may appear in any order.
        if cfg.bind.ip().is_unspecified() && !cfg.allow_wildcard_bind {
            return Err(ConfigError::Value {
                key: "bind",
                line: bind_line.unwrap_or(0),
            });
        }

        Ok(cfg)
    }

    /// Load configuration from a file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_config() {
        let text = "\
# oxagent configuration
bind = 127.0.0.1:9000
token_path = C:\\oxrdp\\token.txt
cert_path = C:\\oxrdp\\cert.pem
key_path = C:\\oxrdp\\key.pem
target_fps = 60
max_frames_in_flight = 3
";
        let cfg = AgentConfig::parse(text).unwrap();
        assert_eq!(cfg.bind, "127.0.0.1:9000".parse().unwrap());
        assert_eq!(cfg.token_path, PathBuf::from("C:\\oxrdp\\token.txt"));
        assert_eq!(cfg.target_fps, 60);
        assert_eq!(cfg.max_frames_in_flight, 3);
    }

    #[test]
    fn applies_defaults_for_absent_keys() {
        let cfg = AgentConfig::parse("token_path = t.txt\n").unwrap();
        let d = AgentConfig::default();
        assert_eq!(cfg.bind, d.bind);
        assert_eq!(cfg.target_fps, d.target_fps);
        assert_eq!(cfg.max_frames_in_flight, d.max_frames_in_flight);
        assert_eq!(cfg.token_path, PathBuf::from("t.txt"));
    }

    #[test]
    fn token_path_is_required() {
        assert!(matches!(
            AgentConfig::parse("target_fps = 30\n"),
            Err(ConfigError::Missing { key: "token_path" })
        ));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let cfg = AgentConfig::parse("\n  # a comment\n\ntoken_path = t\n").unwrap();
        assert_eq!(cfg.token_path, PathBuf::from("t"));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let cfg = AgentConfig::parse("token_path = t\nfuture_option = 1\n").unwrap();
        assert_eq!(cfg.token_path, PathBuf::from("t"));
    }

    #[test]
    fn reports_the_offending_line() {
        assert!(matches!(
            AgentConfig::parse("token_path = t\nnonsense\n"),
            Err(ConfigError::Syntax { line: 2 })
        ));
        assert!(matches!(
            AgentConfig::parse("token_path = t\ntarget_fps = 0\n"),
            Err(ConfigError::Value {
                key: "target_fps",
                line: 2
            })
        ));
        assert!(matches!(
            AgentConfig::parse("token_path = t\nbind = not-an-address\n"),
            Err(ConfigError::Value {
                key: "bind",
                line: 2
            })
        ));
    }

    #[test]
    fn rejects_a_wildcard_bind() {
        for addr in ["0.0.0.0:7644", "[::]:7644"] {
            let text = format!("token_path = t\nbind = {addr}\n");
            assert!(
                matches!(
                    AgentConfig::parse(&text),
                    Err(ConfigError::Value {
                        key: "bind",
                        line: 2
                    })
                ),
                "{addr} must be refused"
            );
        }
    }

    #[test]
    fn a_wildcard_bind_requires_an_explicit_opt_in() {
        // Refused by default, whichever order the keys appear in.
        assert!(matches!(
            AgentConfig::parse("token_path = t\nbind = 0.0.0.0:7644\n"),
            Err(ConfigError::Value { key: "bind", .. })
        ));
        assert!(matches!(
            AgentConfig::parse("bind = [::]:7644\ntoken_path = t\n"),
            Err(ConfigError::Value { key: "bind", .. })
        ));

        // Allowed when the operator says so — the VM-behind-a-port-forward case.
        let cfg =
            AgentConfig::parse("token_path = t\nbind = 0.0.0.0:7644\nallow_wildcard_bind = true\n")
                .unwrap();
        assert!(cfg.bind.ip().is_unspecified());
        assert!(cfg.allow_wildcard_bind);

        // And the opt-in may precede the bind line.
        assert!(AgentConfig::parse(
            "allow_wildcard_bind = true\ntoken_path = t\nbind = 0.0.0.0:7644\n"
        )
        .is_ok());

        // A non-boolean opt-in is a config error, not a silent false.
        assert!(matches!(
            AgentConfig::parse("token_path = t\nallow_wildcard_bind = yes\n"),
            Err(ConfigError::Value {
                key: "allow_wildcard_bind",
                line: 2
            })
        ));
    }

    #[test]
    fn rejects_empty_path_values() {
        assert!(matches!(
            AgentConfig::parse("token_path =\n"),
            Err(ConfigError::Value {
                key: "token_path",
                line: 1
            })
        ));
    }

    #[test]
    fn tolerates_a_utf8_bom() {
        let cfg = AgentConfig::parse("\u{FEFF}token_path = t\n").unwrap();
        assert_eq!(cfg.token_path, PathBuf::from("t"));
    }

    #[test]
    fn default_bind_is_not_a_wildcard() {
        assert!(!AgentConfig::default().bind.ip().is_unspecified());
    }
}
