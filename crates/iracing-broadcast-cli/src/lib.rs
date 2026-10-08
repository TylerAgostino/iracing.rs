mod commands;
mod parser;

use anyhow::Result;
use clap::Subcommand;
use iracing_broadcast_sdk::Command as BroadcastCommand;

mod session_select;

pub use session_select::{parse_session_time, resolve_session};

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Manipulate the camera
    Camera {
        #[command(subcommand)]
        command: commands::CameraCommand,
    },
    /// Modify the replay state
    Replay {
        #[command(subcommand)]
        command: commands::ReplayCommand,
    },
    /// Chat commands
    Chat {
        #[command(subcommand)]
        command: commands::ChatCommand,
    },
    /// Pit service commands
    Pit {
        #[command(subcommand)]
        command: commands::PitCommand,
    },
    /// In-sim car textures
    Textures {
        #[command(subcommand)]
        command: commands::TextureCommand,
    },
    /// Modify the disk-telemetry state
    Telemetry {
        #[command(subcommand)]
        command: commands::TelemetryCommand,
    },
    /// Modify FFB
    Ffb {
        #[command(subcommand)]
        command: commands::ForceFeedbackCommand,
    },
    /// Video and screen capture utilities
    Video {
        #[command(subcommand)]
        command: commands::VideoCommand,
    },
}

impl Command {
    /// Send this command through the Windows broadcast client.
    ///
    /// `replay search-session-time` first resolves its `--session` and
    /// `--time` inputs against the live telemetry session list, then sends
    /// the low-level `ReplaySearchSessionTime` broadcast. The resolution
    /// awaits on the caller's tokio runtime; this must be called from an
    /// async context (or an existing tokio runtime) on Windows.
    ///
    /// # Errors
    ///
    /// Returns an unsupported-platform error on non-Windows systems. On Windows,
    /// propagates live session resolution, client initialization, camera-state
    /// conversion, command encoding, and Win32 dispatch errors.
    pub async fn run(self) -> Result<()> {
        #[cfg(not(windows))]
        {
            Err(anyhow::anyhow!("Broadcast commands only run on windows"))
        }

        #[cfg(windows)]
        {
            let message = match self {
                Command::Replay {
                    command: commands::ReplayCommand::SearchSessionTime { session, time },
                } => resolve_live_search_session_time(&session, &time).await?,
                command => command.try_into()?,
            };
            let client = iracing_broadcast_sdk::Client::new()?;
            client.send_message(message)?;
            Ok(())
        }
    }
}

/// Resolve operator `search-session-time` inputs and build the low-level
/// replay search broadcast command.
#[cfg(windows)]
async fn resolve_live_search_session_time(session: &str, time: &str) -> Result<BroadcastCommand> {
    let session_time_ms = parse_session_time(time)?;
    let session_number = resolve_session_number_against_live(session).await?;
    Ok(BroadcastCommand::ReplaySearchSessionTime(
        session_number,
        session_time_ms,
    ))
}

/// Waits for live session metadata and resolves a session selector to the
/// actual iRacing session number used by the broadcast protocol.
#[cfg(windows)]
async fn resolve_session_number_against_live(selector: &str) -> Result<u16> {
    let connection = iracing_sdk::connections::live::LiveConnection::builder()
        .build()
        .map_err(|error| anyhow::anyhow!("failed to open live telemetry: {error}"))?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Some(session_info) = connection.current_session() {
            let session_num = resolve_session(selector, &session_info.session_info.sessions)?;
            return u16::try_from(session_num).map_err(|_| {
                anyhow::anyhow!("session number {session_num} exceeds the broadcast protocol range")
            });
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(anyhow::anyhow!(
                "timed out waiting for iRacing session metadata; \
                 is iRacing running with a session loaded?"
            ));
        }
        tokio::time::sleep((deadline - now).min(std::time::Duration::from_millis(250))).await;
    }
}

impl TryFrom<Command> for BroadcastCommand {
    type Error = anyhow::Error;

    /// Build the selected SDK command without sending it.
    ///
    /// # Errors
    ///
    /// Propagates replay command conversion errors; `replay search-session-time`
    /// must be sent through [`Command::run`], which resolves its selectors
    /// against live session metadata first.
    fn try_from(command: Command) -> Result<Self, Self::Error> {
        match command {
            Command::Telemetry { command } => Ok(command.into()),
            Command::Ffb { command } => Ok(command.into()),
            Command::Video { command } => Ok(command.into()),
            Command::Textures { command } => Ok(command.into()),
            Command::Chat { command } => Ok(command.into()),
            Command::Camera { command } => Ok(command.into()),
            Command::Replay { command } => command.try_into(),
            Command::Pit { command } => Ok(command.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};
    use iracing_broadcast_sdk::{CameraState, ReplayPositionMode, ReplaySearchMode};

    use super::*;

    #[derive(Parser)]
    #[command(name = "iracing-broadcast")]
    struct TestCli {
        #[command(subcommand)]
        command: Command,
    }

    fn parse_command(args: impl IntoIterator<Item = &'static str>) -> Command {
        TestCli::try_parse_from(std::iter::once("iracing-broadcast").chain(args))
            .expect("command should parse")
            .command
    }

    fn command_parse_fails(args: impl IntoIterator<Item = &'static str>) -> bool {
        TestCli::try_parse_from(std::iter::once("iracing-broadcast").chain(args)).is_err()
    }

    #[test]
    fn command_definition_is_valid() {
        TestCli::command().debug_assert();
    }

    #[test]
    fn camera_state_accepts_repeated_named_flags() {
        let command = parse_command([
            "camera",
            "set-state",
            "--flag",
            "ui-hidden",
            "--flag",
            "use-mouse-aim",
        ]);

        let expected = CameraState::USER_INTERFACE_HIDDEN.union(CameraState::USE_MOUSE_AIM_MODE);
        assert_eq!(
            BroadcastCommand::try_from(command).unwrap(),
            BroadcastCommand::CameraSetState(expected)
        );
    }

    #[test]
    fn camera_state_accepts_raw_bits() {
        let command = parse_command(["camera", "set-state", "--raw-bits", "8"]);

        assert_eq!(
            BroadcastCommand::try_from(command).unwrap(),
            BroadcastCommand::CameraSetState(CameraState::from_bits_retain(8))
        );
    }

    #[test]
    fn camera_state_rejects_raw_bits_with_named_flags() {
        assert!(command_parse_fails([
            "camera",
            "set-state",
            "--raw-bits",
            "8",
            "--flag",
            "ui-hidden",
        ]));
    }

    #[test]
    fn camera_state_requires_raw_bits_or_named_flags() {
        assert!(command_parse_fails(["camera", "set-state"]));
    }

    #[test]
    fn replay_search_parses_domain_mode() {
        let command = parse_command(["replay", "search", "previous-session"]);

        assert_eq!(
            BroadcastCommand::try_from(command).unwrap(),
            BroadcastCommand::ReplaySearch(ReplaySearchMode::PreviousSession)
        );
    }

    #[test]
    fn replay_position_parses_domain_mode() {
        let command = parse_command(["replay", "set-play-position", "current", "--frame", "123"]);

        assert_eq!(
            BroadcastCommand::try_from(command).unwrap(),
            BroadcastCommand::ReplaySetPlayPosition(ReplayPositionMode::Current, 123)
        );
    }
}
