use clap::{Parser, Subcommand, ValueEnum};

use crate::core::task::InterfaceMode;

#[derive(Parser, Debug)]
#[command(
    name = "jumabek",
    about = "JumaBek — your machine, spoken to",
    long_about = None,
    version
)]
pub struct Args {
    #[arg(short, long, value_delimiter = ',', value_name = "MODE")]
    pub mode: Vec<Mode>,

    #[arg(short = 'v', long, conflicts_with = "cli")]
    pub voice: bool,

    #[arg(long)]
    pub cli: bool,

    #[arg(short, long, value_name = "PATH")]
    pub config: Option<String>,

    #[arg(trailing_var_arg = true, value_name = "TASK")]
    pub task: Vec<String>,

    #[command(subcommand)]
    pub command: Option<Manage>,
}

#[derive(Subcommand, Debug)]
pub enum Manage {
    Skills,
    Remove {
        name: String,
    },
    Backups,
    Restore {
        id: String,
    },
    Jobs,
    JobStop {
        id: i64,
    },
    Agents {
        #[arg(long)]
        once: bool,
    },
    Rights,
    Tokens,
    Inbox,
    Profile,
    ForgetSubject {
        subject: String,
    },
    Mic {
        #[arg(default_value_t = 10)]
        seconds: u64,
    },
    Doctor,
    Where,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    Cli,
    Voice,
}

impl Mode {
    pub fn as_interface_mode(&self) -> InterfaceMode {
        match self {
            Mode::Cli => InterfaceMode::Cli,
            Mode::Voice => InterfaceMode::Voice,
        }
    }

    pub fn parse(name: &str) -> Option<Mode> {
        match name.trim().to_lowercase().as_str() {
            "cli" | "text" | "текст" => Some(Mode::Cli),
            "voice" | "голос" => Some(Mode::Voice),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Cli => "cli",
            Mode::Voice => "voice",
        }
    }
}

impl Args {
    pub fn one_shot_task(&self) -> Option<String> {
        if self.task.is_empty() {
            return None;
        }

        let joined = self.task.join(" ").trim().to_string();
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    }

    pub fn requested_mode(&self) -> Option<Mode> {
        if let Some(mode) = self.mode.first().copied() {
            return Some(mode);
        }
        if self.voice {
            return Some(Mode::Voice);
        }
        if self.cli {
            return Some(Mode::Cli);
        }
        None
    }

    pub fn flag_like_task(&self) -> Option<&str> {
        self.task
            .iter()
            .find(|word| word.starts_with('-') && word.len() > 1)
            .map(|word| word.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Args {
        Args::parse_from(std::iter::once("jumabek").chain(args.iter().copied()))
    }

    #[test]
    fn defaults_to_no_override() {
        let args = parse(&[]);
        assert!(args.requested_mode().is_none());
        assert!(args.one_shot_task().is_none());
    }

    #[test]
    fn reads_the_mode_flag() {
        assert_eq!(
            parse(&["--mode", "voice"]).requested_mode(),
            Some(Mode::Voice)
        );
        assert_eq!(parse(&["-m", "cli"]).requested_mode(), Some(Mode::Cli));
    }

    #[test]
    fn accepts_several_modes_and_takes_the_first() {
        let args = parse(&["--mode", "voice,cli"]);
        assert_eq!(args.mode, vec![Mode::Voice, Mode::Cli]);
        assert_eq!(args.requested_mode(), Some(Mode::Voice));
    }

    #[test]
    fn collects_a_one_shot_task() {
        let args = parse(&["сколько", "файлов", "в", "папке"]);
        assert_eq!(args.one_shot_task().unwrap(), "сколько файлов в папке");
    }

    #[test]
    fn combines_mode_and_task() {
        let args = parse(&["--mode", "voice", "открой", "файл"]);
        assert_eq!(args.requested_mode(), Some(Mode::Voice));
        assert_eq!(args.one_shot_task().unwrap(), "открой файл");
    }

    #[test]
    fn voice_and_cli_are_flags_of_their_own() {
        assert_eq!(parse(&["--voice"]).requested_mode(), Some(Mode::Voice));
        assert_eq!(parse(&["-v"]).requested_mode(), Some(Mode::Voice));
        assert_eq!(parse(&["--cli"]).requested_mode(), Some(Mode::Cli));
    }

    #[test]
    fn an_explicit_mode_beats_the_shorthand() {
        let args = parse(&["--mode", "cli", "--voice"]);
        assert_eq!(args.requested_mode(), Some(Mode::Cli));
    }

    #[test]
    fn voice_and_cli_together_are_refused() {
        let parsed = Args::try_parse_from(["jumabek", "--voice", "--cli"]);
        assert!(
            parsed.is_err(),
            "asking for both modes at once was accepted"
        );
    }

    #[test]
    fn a_flag_that_landed_in_the_task_is_spotted() {
        let args = parse(&["--", "--voice"]);
        assert_eq!(args.flag_like_task(), Some("--voice"));
        assert_eq!(args.one_shot_task().unwrap(), "--voice");
    }

    #[test]
    fn an_ordinary_task_is_not_mistaken_for_a_flag() {
        assert!(parse(&["посчитай", "5-3"]).flag_like_task().is_none());
        assert!(
            parse(&["what", "is", "-", "for?"])
                .flag_like_task()
                .is_none()
        );
    }

    #[test]
    fn parses_mode_names_typed_at_runtime() {
        assert_eq!(Mode::parse("voice"), Some(Mode::Voice));
        assert_eq!(Mode::parse(" ГОЛОС "), Some(Mode::Voice));
        assert_eq!(Mode::parse("text"), Some(Mode::Cli));
        assert_eq!(Mode::parse("телепатия"), None);
    }
}
