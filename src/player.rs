use std::process::{Command, Stdio};
use tokio::process::Command as TokioCommand;

#[derive(Clone, Copy)]
pub enum PlayerType {
    Ffplay,
    Mpv,
    Afplay,
}

pub struct VolumeControl {
    pub volume: u32, // 0-100
    pub player_type: PlayerType,
    pub mpv_socket: Option<String>,
    pub muted: bool,
    volume_before_mute: u32,
}

impl VolumeControl {
    pub fn new(player_type: PlayerType) -> Self {
        Self {
            volume: 70,
            player_type,
            mpv_socket: None,
            muted: false,
            volume_before_mute: 70,
        }
    }

    pub fn increase_volume(&mut self) {
        self.volume = (self.volume + 5).min(100);
    }

    pub fn decrease_volume(&mut self) {
        self.volume = self.volume.saturating_sub(5);
    }

    pub fn toggle_mute(&mut self) {
        if self.muted {
            self.volume = self.volume_before_mute;
            self.muted = false;
        } else {
            self.volume_before_mute = self.volume;
            self.volume = 0;
            self.muted = true;
        }
    }

    pub async fn apply_mute(
        &self,
        child: &mut tokio::process::Child,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match self.player_type {
            PlayerType::Mpv => {
                if let Some(ref socket) = self.mpv_socket {
                    let cmd = if self.muted {
                        "set pause yes\n"
                    } else {
                        "set pause no\n"
                    };
                    if let Ok(mut stream) = tokio::net::UnixStream::connect(socket).await {
                        use tokio::io::AsyncWriteExt;
                        stream.write_all(cmd.as_bytes()).await?;
                        return Ok(());
                    }
                }
                self.apply_volume(child).await
            }
            PlayerType::Ffplay => self.apply_volume(child).await,
            PlayerType::Afplay => {
                #[cfg(unix)]
                {
                    use nix::sys::signal;
                    use nix::unistd::Pid;

                    if let Some(pid) = child.id() {
                        let sig = if self.muted {
                            signal::Signal::SIGSTOP
                        } else {
                            signal::Signal::SIGCONT
                        };
                        let _ = signal::kill(Pid::from_raw(pid as i32), sig);
                        return Ok(());
                    }
                }
                self.apply_volume(child).await
            }
        }
    }

    pub async fn apply_volume(
        &self,
        _child: &mut tokio::process::Child,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match self.player_type {
            PlayerType::Mpv => {
                if let Some(ref socket) = self.mpv_socket {
                    let volume_cmd = format!("set volume {}%\n", self.volume);
                    if let Ok(mut stream) = tokio::net::UnixStream::connect(socket).await {
                        use tokio::io::AsyncWriteExt;
                        stream.write_all(volume_cmd.as_bytes()).await?;
                    }
                } else {
                    return Err("MPV IPC not available, restart needed".into());
                }
            }
            PlayerType::Ffplay => {
                return Err("FFplay restart needed".into());
            }
            PlayerType::Afplay => {
                if cfg!(target_os = "macos") {
                    let script =
                        format!("set volume output volume {}", self.volume);
                    let _ = Command::new("osascript")
                        .arg("-e")
                        .arg(script)
                        .output();
                }
            }
        }
        Ok(())
    }
}

pub fn detect_player() -> Option<PlayerType> {
    if Command::new("mpv").arg("--version").output().is_ok() {
        Some(PlayerType::Mpv)
    } else if Command::new("ffplay").arg("-version").output().is_ok() {
        Some(PlayerType::Ffplay)
    } else if cfg!(target_os = "macos")
        && Command::new("afplay").arg("--help").output().is_ok()
        && Command::new("curl").arg("--version").output().is_ok()
    {
        Some(PlayerType::Afplay)
    } else {
        None
    }
}

/// Returns `(command, args, optional_ipc_socket_path)`.
pub fn build_player_args(
    player_type: PlayerType,
    stream_url: &str,
    volume: u32,
) -> (String, Vec<String>, Option<String>) {
    match player_type {
        PlayerType::Ffplay => (
            "ffplay".to_string(),
            vec![
                "-nodisp".to_string(),
                "-loglevel".to_string(),
                "quiet".to_string(),
                "-reconnect".to_string(),
                "1".to_string(),
                "-reconnect_streamed".to_string(),
                "1".to_string(),
                "-reconnect_delay_max".to_string(),
                "5".to_string(),
                "-volume".to_string(),
                volume.to_string(),
                stream_url.to_string(),
            ],
            None,
        ),
        PlayerType::Mpv => {
            let socket_path = format!("/tmp/mpv_lofi_{}.sock", std::process::id());
            (
                "mpv".to_string(),
                vec![
                    "--no-video".to_string(),
                    "--no-terminal".to_string(),
                    "--quiet".to_string(),
                    "--stream-lavf-o=reconnect=1,reconnect_streamed=1,reconnect_delay_max=5"
                        .to_string(),
                    format!("--input-ipc-server={}", socket_path),
                    format!("--volume={}", volume),
                    stream_url.to_string(),
                ],
                Some(socket_path),
            )
        }
        PlayerType::Afplay => {
            let curl_cmd = format!(
                "while true; do curl -fsSL --retry 5 --retry-delay 1 '{}' | afplay -; sleep 1; done",
                stream_url
            );
            ("sh".to_string(), vec!["-c".to_string(), curl_cmd], None)
        }
    }
}

/// Spawn a player child process with all stdio suppressed.
pub async fn spawn_player(
    cmd: &str,
    args: &[String],
) -> Result<tokio::process::Child, std::io::Error> {
    TokioCommand::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}
