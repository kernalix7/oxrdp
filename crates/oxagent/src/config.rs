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
        }
    }
}

impl AgentConfig {
    /// Parse configuration from the text of a config file.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut cfg = AgentConfig::default();
        let mut token_path_seen = false;

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
                "bind" => match value.parse::<SocketAddr>() {
                    Ok(addr) => cfg.bind = addr,
                    Err(_) => {
                        return Err(ConfigError::Value {
                            key: "bind",
                            line: line_no,
                        })
                    }
                },
                "token_path" => {
                    cfg.token_path = PathBuf::from(value);
                    token_path_seen = true;
                }
                "cert_path" => cfg.cert_path = PathBuf::from(value),
                "key_path" => cfg.key_path = PathBuf::from(value),
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
    fn default_bind_is_not_a_wildcard() {
        assert!(!AgentConfig::default().bind.ip().is_unspecified());
    }
}
