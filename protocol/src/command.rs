//! The injector → agent_loader command set and its wire encoding.

use std::path::PathBuf;

use thiserror::Error;

/// A single command sent from the injector to the agent loader.
///
/// The wire form is one UTF-8 line: a lowercase verb followed by a space and
/// its arguments. `Reload` carries two paths — the client library and the
/// injector's working directory — separated by a tab, so each path may still
/// contain spaces and survive the round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Load — or hot-reload — the client library, and tell the client where
    /// to keep its config (the injector's working directory).
    Reload {
        /// Absolute path of the client library.
        library: PathBuf,
        /// Directory the injector was started in — where the client config
        /// file is read from and written to.
        config_dir: PathBuf,
    },
    /// Ask the running client to write its current state to its config file
    /// right now. The agent answers with one `ok` / `err …` line.
    SaveConfig,
    /// Ask the running client to re-read its config file and apply it live —
    /// settings, keybinds, layout, and enabling/disabling modules. The agent
    /// answers with one `ok` / `err …` line.
    LoadConfig,
}

/// Failure to parse a command off the wire.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    /// The line carried no verb.
    #[error("empty command line")]
    Empty,
    /// The verb is not one this protocol version understands.
    #[error("unknown command verb: {0:?}")]
    UnknownVerb(String),
    /// A verb that requires an argument was sent without one.
    #[error("command {verb:?} is missing its argument")]
    MissingArgument { verb: &'static str },
}

impl Command {
    /// Serializes the command into its single-line wire form (no newline).
    pub fn encode(&self) -> String {
        match self {
            Command::Reload {
                library,
                config_dir,
            } => format!("reload {}\t{}", library.display(), config_dir.display()),
            Command::SaveConfig => "save-config".to_string(),
            Command::LoadConfig => "load-config".to_string(),
        }
    }

    /// Parses one wire line back into a [`Command`].
    ///
    /// The argument runs to the end of the line, so paths containing spaces
    /// survive the round trip.
    pub fn decode(line: &str) -> Result<Self, ProtocolError> {
        let line = line.trim();
        if line.is_empty() {
            return Err(ProtocolError::Empty);
        }
        let (verb, arg) = match line.split_once(' ') {
            Some((verb, arg)) => (verb, arg.trim()),
            None => (line, ""),
        };
        match verb {
            "reload" => {
                if arg.is_empty() {
                    return Err(ProtocolError::MissingArgument { verb: "reload" });
                }
                // The library path and the config directory are tab-separated;
                // an old-style line without a tab still decodes (no config dir).
                let (library, config_dir) = match arg.split_once('\t') {
                    Some((library, config_dir)) => {
                        (PathBuf::from(library), PathBuf::from(config_dir))
                    }
                    None => (PathBuf::from(arg), PathBuf::new()),
                };
                Ok(Command::Reload {
                    library,
                    config_dir,
                })
            }
            "save-config" => Ok(Command::SaveConfig),
            "load-config" => Ok(Command::LoadConfig),
            other => Err(ProtocolError::UnknownVerb(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn reload_round_trips() {
        let command = Command::Reload {
            library: PathBuf::from("/tmp/libclient.so"),
            config_dir: PathBuf::from("/home/work"),
        };
        let wire = command.encode();
        assert_eq!(wire, "reload /tmp/libclient.so\t/home/work");
        assert_eq!(Command::decode(&wire), Ok(command));
    }

    #[test]
    fn reload_paths_with_spaces_survive_the_round_trip() {
        let command = Command::Reload {
            library: PathBuf::from("/home/My Games/libclient.so"),
            config_dir: PathBuf::from("/home/My Games/cfg dir"),
        };
        assert_eq!(Command::decode(&command.encode()), Ok(command));
    }

    #[test]
    fn decode_ignores_surrounding_whitespace() {
        assert_eq!(
            Command::decode("  reload /a/b.so\t/c\n"),
            Ok(Command::Reload {
                library: PathBuf::from("/a/b.so"),
                config_dir: PathBuf::from("/c"),
            }),
        );
    }

    #[test]
    fn reload_without_a_config_dir_decodes_with_an_empty_one() {
        assert_eq!(
            Command::decode("reload /a/b.so"),
            Ok(Command::Reload {
                library: PathBuf::from("/a/b.so"),
                config_dir: PathBuf::new(),
            }),
        );
    }

    #[test]
    fn config_commands_round_trip() {
        for command in [Command::SaveConfig, Command::LoadConfig] {
            assert_eq!(Command::decode(&command.encode()), Ok(command));
        }
    }

    #[test]
    fn an_empty_line_is_rejected() {
        assert_eq!(Command::decode("   "), Err(ProtocolError::Empty));
    }

    #[test]
    fn reload_without_an_argument_is_rejected() {
        assert_eq!(
            Command::decode("reload"),
            Err(ProtocolError::MissingArgument { verb: "reload" }),
        );
    }

    #[test]
    fn an_unknown_verb_is_rejected() {
        assert_eq!(
            Command::decode("frobnicate x"),
            Err(ProtocolError::UnknownVerb("frobnicate".to_string())),
        );
    }
}
