//! Command-line options. A setting given on the command line overrides
//! config.toml, which overrides the built-in default.

use clap::Parser;

use crate::usbip::server::PlaybackCompletion;

#[derive(Parser, Debug, Default, PartialEq)]
#[command(
    name = "VirtualCables",
    about = "Virtual audio cables exported as USB Audio Class devices over USB/IP.",
    after_help = "Settings not given on the command line are read from config.toml in the settings folder.",
    disable_version_flag = true
)]
pub struct Cli {
    /// USB/IP listen address [config.toml: listen, default 127.0.0.1:3240]
    #[arg(long, value_name = "ADDRESS")]
    pub listen: Option<String>,

    /// Number of virtual cables, 1-32, with IDs from 1 and default names [config.toml: [[cable]] tables, default none]
    #[arg(long, value_name = "COUNT")]
    pub cables: Option<i64>,

    /// Delay between each cable's playback and recording sides, 1-1000 ms [config.toml: latency_ms, default 40]
    #[arg(long, value_name = "MS")]
    pub latency_ms: Option<i64>,

    /// When playback transfers complete: end (of their last frame) or start (of their first) [config.toml: playback_completion, default end]
    #[arg(long, value_name = "WHEN", value_parser = parse_playback_completion)]
    pub playback_completion: Option<PlaybackCompletion>,

    /// Log USB/IP stream traces and pipe-reset events for troubleshooting [config.toml: diagnostics, default false]
    #[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true", value_parser = parse_bool_arg)]
    pub diagnostics: Option<bool>,

    /// Hold all audio transfers this many ms once a minute, to reproduce a system stall; 0 is off
    /// [config.toml: stall_test_ms, default 0]
    #[arg(long, value_name = "MS")]
    pub stall_test_ms: Option<i64>,

    /// Validate descriptors and the audio path, then exit
    #[arg(long)]
    pub self_test: bool,

    /// Print USB descriptors, then exit
    #[arg(long)]
    pub dump_descriptors: bool,

    /// Print version, then exit
    #[arg(long, short = 'V')]
    pub version: bool,

    /// Run the elevated cable-device broker (started by the application itself)
    #[arg(long, hide = true)]
    pub attach_broker: bool,

    /// Broker callback address (internal)
    #[arg(long, hide = true)]
    pub broker_address: Option<String>,

    /// Broker authentication token (internal)
    #[arg(long, hide = true)]
    pub broker_token: Option<String>,
}

impl Cli {
    /// Whether the options select a one-off task instead of the application.
    pub fn is_helper_mode(&self) -> bool {
        self.attach_broker || self.self_test || self.dump_descriptors || self.version
    }
}

fn parse_bool_arg(value: &str) -> Result<bool, String> {
    crate::config::parse_bool(value).ok_or_else(|| format!("expected true or false, got {value:?}"))
}

fn parse_playback_completion(value: &str) -> Result<PlaybackCompletion, String> {
    PlaybackCompletion::parse(value).ok_or_else(|| format!("expected end or start, got {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("virtualcables").chain(args.iter().copied()))
    }

    #[test]
    fn parses_options() {
        let cli = parse(&["--cables", "3", "--listen=0.0.0.0:1", "--latency-ms", "25", "--diagnostics"]).unwrap();
        assert_eq!(cli.cables, Some(3));
        assert_eq!(cli.listen.as_deref(), Some("0.0.0.0:1"));
        assert_eq!(cli.latency_ms, Some(25));
        assert_eq!(cli.diagnostics, Some(true));
        assert_eq!(cli.stall_test_ms, None, "unset options fall back to config.toml");

        assert_eq!(parse(&["--diagnostics=false"]).unwrap().diagnostics, Some(false));
        let cli = parse(&["--playback-completion", "start", "--stall-test-ms", "40"]).unwrap();
        assert_eq!((cli.playback_completion, cli.stall_test_ms), (Some(PlaybackCompletion::Start), Some(40)));
        assert_eq!(parse(&[]).unwrap(), Cli::default());
    }

    #[test]
    fn broker_launch_arguments_parse() {
        // The exact arguments the application passes to its elevated copy.
        let cli = parse(&["--attach-broker", "--broker-address", "127.0.0.1:5000", "--broker-token", "ab"]).unwrap();
        assert!(cli.attach_broker);
        assert_eq!(cli.broker_address.as_deref(), Some("127.0.0.1:5000"));
        assert_eq!(cli.broker_token.as_deref(), Some("ab"));
    }

    #[test]
    fn rejects_bad_input() {
        for args in [
            &["--cables", "x"][..],
            &["--bogus"],
            &["--cables"],
            &["--diagnostics=maybe"],
            &["--playback-completion", "middle"],
        ] {
            let err = parse(args).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{args:?}");
        }
        assert_eq!(parse(&["--help"]).unwrap_err().exit_code(), 0);
    }

    #[test]
    fn help_shows_the_real_defaults() {
        use crate::config::DEFAULT_LISTEN;
        use crate::uac2::{DEFAULT_LATENCY_MS, MAX_LATENCY_MS, MIN_LATENCY_MS};
        let help = Cli::command().render_help().to_string();
        for expected in [
            format!("default {DEFAULT_LISTEN}"),
            "[[cable]] tables, default none]".to_string(),
            format!("{MIN_LATENCY_MS}-{MAX_LATENCY_MS} ms"),
            format!("latency_ms, default {DEFAULT_LATENCY_MS}]"),
            format!("default {}]", PlaybackCompletion::default().name()),
        ] {
            assert!(help.contains(&expected), "help text lacks {expected:?}:\n{help}");
        }
        assert!(!help.contains("broker"), "internal options must stay hidden");
        Cli::command().debug_assert();
    }

    #[test]
    fn helper_modes() {
        assert!(!parse(&[]).unwrap().is_helper_mode());
        assert!(!parse(&["--cables", "2", "--diagnostics"]).unwrap().is_helper_mode());
        for args in [&["--self-test"][..], &["--dump-descriptors"], &["--version"], &["-V"], &["--attach-broker"]] {
            assert!(parse(args).unwrap().is_helper_mode(), "{args:?}");
        }
    }
}
