mod ui;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::{cursor::{Hide, Show, MoveTo}, execute};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Instant;
use tokio::process::Command as TokioCommand;
use tokio::sync::Mutex;

use crate::ui::{UiState, draw_ui, STATIONS};

#[derive(Clone, Copy)]
enum PlayerType {
    Ffplay,
    Mpv,
    Afplay,
}

struct VolumeControl {
    volume: u32, // 0-100
    player_type: PlayerType,
    mpv_socket: Option<String>,
    muted: bool,
    volume_before_mute: u32, // Store volume before muting
}

impl VolumeControl {
    fn new(player_type: PlayerType) -> Self {
        Self {
            volume: 70, // Default volume
            player_type,
            mpv_socket: None,
            muted: false,
            volume_before_mute: 70,
        }
    }

    fn increase_volume(&mut self) {
        self.volume = (self.volume + 5).min(100);
    }

    fn decrease_volume(&mut self) {
        self.volume = self.volume.saturating_sub(5);
    }

    fn toggle_mute(&mut self) {
        if self.muted {
            // Unmute: restore previous volume
            self.volume = self.volume_before_mute;
            self.muted = false;
        } else {
            // Mute: save current volume and set to 0
            self.volume_before_mute = self.volume;
            self.volume = 0;
            self.muted = true;
        }
    }

    async fn apply_mute(&self, child: &mut tokio::process::Child) -> Result<(), Box<dyn std::error::Error>> {
        match self.player_type {
            PlayerType::Mpv => {
                // Use IPC to pause/unpause
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
                // If IPC fails, use volume control
                self.apply_volume(child).await
            }
            PlayerType::Ffplay => {
                // ffplay doesn't support pause easily, use volume control
                self.apply_volume(child).await
            }
            PlayerType::Afplay => {
                // For afplay, we can pause the process with signals
                #[cfg(unix)]
                {
                    use nix::sys::signal;
                    use nix::unistd::Pid;
                    
                    if let Some(pid) = child.id() {
                        if self.muted {
                            let _ = signal::kill(
                                Pid::from_raw(pid as i32),
                                signal::Signal::SIGSTOP,
                            );
                        } else {
                            let _ = signal::kill(
                                Pid::from_raw(pid as i32),
                                signal::Signal::SIGCONT,
                            );
                        }
                        return Ok(());
                    }
                }
                // Fallback: use volume control
                self.apply_volume(child).await
            }
        }
    }

    async fn apply_volume(&self, _child: &mut tokio::process::Child) -> Result<(), Box<dyn std::error::Error>> {
        match self.player_type {
            PlayerType::Mpv => {
                // Try to use IPC socket if available
                if let Some(ref socket) = self.mpv_socket {
                    let volume_cmd = format!("set volume {}%\n", self.volume);
                    if let Ok(mut stream) = tokio::net::UnixStream::connect(socket).await {
                        use tokio::io::AsyncWriteExt;
                        stream.write_all(volume_cmd.as_bytes()).await?;
                    }
                } else {
                    // Fallback: restart with new volume
                    return Err("MPV IPC not available, restart needed".into());
                }
            }
            PlayerType::Ffplay => {
                // ffplay doesn't support runtime volume control, need to restart
                return Err("FFplay restart needed".into());
            }
            PlayerType::Afplay => {
                // Control macOS system volume
                if cfg!(target_os = "macos") {
                    let volume_float = self.volume as f64 / 100.0;
                    let script = format!(
                        "set volume output volume {}",
                        (volume_float * 100.0) as i32
                    );
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

fn build_player_args(player_type: PlayerType, stream_url: &str, volume: u32) -> (String, Vec<String>, Option<String>) {
    match player_type {
        PlayerType::Ffplay => (
            "ffplay".to_string(),
            vec![
                "-nodisp".to_string(),
                "-autoexit".to_string(),
                "-loglevel".to_string(),
                "quiet".to_string(),
                "-volume".to_string(),
                volume.to_string(),
                stream_url.to_string(),
            ],
            None,
        ),
        PlayerType::Mpv => {
            // Create IPC socket path
            let socket_path = format!("/tmp/mpv_lofi_{}.sock", std::process::id());
            (
                "mpv".to_string(),
                vec![
                    "--no-video".to_string(),
                    "--quiet".to_string(),
                    "--input-ipc-server".to_string(),
                    socket_path.clone(),
                    "--volume".to_string(),
                    volume.to_string(),
                    stream_url.to_string(),
                ],
                Some(socket_path),
            )
        }
        PlayerType::Afplay => {
            let curl_cmd = format!("curl -s '{}' | afplay -", stream_url);
            ("sh".to_string(), vec!["-c".to_string(), curl_cmd], None)
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut station_index: usize = 0;
    let mut stream_url: &str = STATIONS[station_index].url;
    let mut ui_state = UiState::new();
    ui_state.station_index = station_index;
    
    // Start UI (drawn after raw mode)
    
    // Detect available player
    let player_type = if Command::new("ffplay").arg("-version").output().is_ok() {
        PlayerType::Ffplay
    } else if Command::new("mpv").arg("--version").output().is_ok() {
        PlayerType::Mpv
    } else if cfg!(target_os = "macos") 
        && Command::new("afplay").arg("--help").output().is_ok() 
        && Command::new("curl").arg("--version").output().is_ok() {
        PlayerType::Afplay
    } else {
        eprintln!("Error: No suitable player found");
        eprintln!("Please install one of the following:");
        if cfg!(target_os = "macos") {
            eprintln!("  macOS: brew install ffmpeg or brew install mpv");
        } else {
            eprintln!("  Linux: sudo apt-get install ffmpeg or sudo apt-get install mpv");
        }
        return Err("Player not found".into());
    };
    
    let _player_name = match player_type {
        PlayerType::Ffplay => "ffplay",
        PlayerType::Mpv => "mpv",
        PlayerType::Afplay => "curl + afplay",
    };
    
    // Initialize volume control
    let mut volume_control = VolumeControl::new(player_type);
    
    // Build player command
    let (player_cmd, player_args, socket_path) = build_player_args(player_type, stream_url, volume_control.volume);
    
    // Set socket path for MPV
    if let Some(socket) = socket_path {
        volume_control.mpv_socket = Some(socket);
    }
    
    let volume_control = Arc::new(Mutex::new(volume_control));
    
    // Enable raw mode for keyboard input
    enable_raw_mode()?;
    // Hide cursor for cleaner interactive output
    {
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, Hide, Clear(ClearType::All), MoveTo(0, 0));
    }

    let stdout = std::io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    draw_ui(&mut terminal, &ui_state, STATIONS);
    
    // Start the player process
    let mut child = TokioCommand::new(&player_cmd)
        .args(&player_args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // Initial UI render
    let vol = volume_control.lock().await;
    ui_state.volume = vol.volume;
    ui_state.muted = vol.muted;
    drop(vol);
    draw_ui(&mut terminal, &ui_state, STATIONS);
    
    // Record start time
    let start_time = Instant::now();
    // Initial status render
    {
        let elapsed = start_time.elapsed();
        ui_state.elapsed = elapsed;
        let vol = volume_control.lock().await;
        ui_state.volume = vol.volume;
        ui_state.muted = vol.muted;
        drop(vol);
        draw_ui(&mut terminal, &ui_state, STATIONS);
    }
    
    // Handle Ctrl+C
    #[cfg(unix)]
    let mut ctrl_c = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    
    // Main event loop
    loop {
        #[cfg(unix)]
        tokio::select! {
            // Check if the process has finished
            result = child.wait() => {
                match result {
                    Ok(status) => {
                        if status.success() {
                            println!("\n\nPlayback finished");
                        } else {
                            println!("\n\nPlayback stopped");
                        }
                        break;
                    }
                    Err(e) => {
                        eprintln!("\n\nError: {}", e);
                        break;
                    }
                }
            }
            // Handle Ctrl+C
            _ = ctrl_c.recv() => {
                println!("\n\nStopping...");
                let _ = child.kill().await;
                break;
            }
            // Handle keyboard input
            key_result = tokio::task::spawn_blocking(|| {
                if event::poll(std::time::Duration::from_millis(100)).unwrap_or(false) {
                    if let Ok(Event::Key(KeyEvent { code, kind, modifiers, .. })) = event::read() {
                        if kind == KeyEventKind::Press {
                            return Some((code, modifiers));
                        }
                    }
                }
                None
            }) => {
                if let Ok(Some((key_code, modifiers))) = key_result {
                     match key_code {
                         KeyCode::F(11) | KeyCode::Up => {
                             let mut vol = volume_control.lock().await;
                             if !vol.muted {
                                 vol.increase_volume();
                             }
                             let current_vol = vol.volume;
                             drop(vol);

                             // Try to apply volume change
                             let vol = volume_control.lock().await;
                             let needs_restart = vol.apply_volume(&mut child).await.is_err();
                             let player_type = vol.player_type;
                             drop(vol);

                             if needs_restart {
                                 // If runtime control failed, restart player
                                 let _ = child.start_kill();
                                 let _ = tokio::time::timeout(
                                     tokio::time::Duration::from_millis(500),
                                     child.wait()
                                 ).await;
                                 let (cmd, args, new_socket) = build_player_args(player_type, stream_url, current_vol);
                                 if let Some(s) = new_socket {
                                     let mut vol = volume_control.lock().await;
                                     vol.mpv_socket = Some(s);
                                     drop(vol);
                                 }
                                 child = TokioCommand::new(&cmd)
                                     .args(&args)
                                     .stdout(Stdio::null())
                                     .stderr(Stdio::null())
                                     .spawn()?;
                             }
                             // Update UI
                             let vol = volume_control.lock().await;
                             ui_state.volume = vol.volume;
                             ui_state.muted = vol.muted;
                             drop(vol);
                             draw_ui(&mut terminal, &ui_state, STATIONS);
                         }
                         KeyCode::F(10) | KeyCode::Down => {
                             let mut vol = volume_control.lock().await;
                             if !vol.muted {
                                 vol.decrease_volume();
                             }
                             let current_vol = vol.volume;
                             drop(vol);

                             // Try to apply volume change
                             let vol = volume_control.lock().await;
                             let needs_restart = vol.apply_volume(&mut child).await.is_err();
                             let player_type = vol.player_type;
                             drop(vol);

                             if needs_restart {
                                 // If runtime control failed, restart player
                                 let _ = child.start_kill();
                                 let _ = tokio::time::timeout(
                                     tokio::time::Duration::from_millis(500),
                                     child.wait()
                                 ).await;
                                 let (cmd, args, new_socket) = build_player_args(player_type, stream_url, current_vol);
                                 if let Some(s) = new_socket {
                                     let mut vol = volume_control.lock().await;
                                     vol.mpv_socket = Some(s);
                                     drop(vol);
                                 }
                                 child = TokioCommand::new(&cmd)
                                     .args(&args)
                                     .stdout(Stdio::null())
                                     .stderr(Stdio::null())
                                     .spawn()?;
                             }
                             // Update UI
                             let vol = volume_control.lock().await;
                             ui_state.volume = vol.volume;
                             ui_state.muted = vol.muted;
                             drop(vol);
                             draw_ui(&mut terminal, &ui_state, STATIONS);
                         }
                          KeyCode::F(7) | KeyCode::Left => {
                              let vol = volume_control.lock().await;
                              let current_vol = vol.volume;
                              let is_muted = vol.muted;
                              let player_type = vol.player_type;
                              drop(vol);

                              station_index = if station_index == 0 { STATIONS.len() - 1 } else { station_index - 1 };
                              stream_url = STATIONS[station_index].url;

                              let _ = child.start_kill();
                              let _ = tokio::time::timeout(
                                  tokio::time::Duration::from_millis(500),
                                  child.wait()
                              ).await;
                              let (cmd, args, new_socket) = build_player_args(player_type, stream_url, current_vol);
                              if let Some(s) = new_socket {
                                  let mut vol = volume_control.lock().await;
                                  vol.mpv_socket = Some(s);
                                  drop(vol);
                              }
                              child = TokioCommand::new(&cmd)
                                  .args(&args)
                                  .stdout(Stdio::null())
                                  .stderr(Stdio::null())
                                  .spawn()?;

                              if is_muted {
                                  let mut vol = volume_control.lock().await;
                                  vol.muted = true;
                                  drop(vol);
                                  let vol2 = volume_control.lock().await;
                                  let _ = vol2.apply_mute(&mut child).await;
                                  drop(vol2);
                              }
                              ui_state.station_index = station_index;
                              draw_ui(&mut terminal, &ui_state, STATIONS);
                          }
                          KeyCode::F(9) | KeyCode::Right => {
                              let vol = volume_control.lock().await;
                              let current_vol = vol.volume;
                              let is_muted = vol.muted;
                              let player_type = vol.player_type;
                              drop(vol);

                              station_index = (station_index + 1) % STATIONS.len();
                              stream_url = STATIONS[station_index].url;

                              let _ = child.start_kill();
                              let _ = tokio::time::timeout(
                                  tokio::time::Duration::from_millis(500),
                                  child.wait()
                              ).await;
                              let (cmd, args, new_socket) = build_player_args(player_type, stream_url, current_vol);
                              if let Some(s) = new_socket {
                                  let mut vol = volume_control.lock().await;
                                  vol.mpv_socket = Some(s);
                                  drop(vol);
                                  }
                              child = TokioCommand::new(&cmd)
                                  .args(&args)
                                  .stdout(Stdio::null())
                                  .stderr(Stdio::null())
                                  .spawn()?;

                              if is_muted {
                                  let mut vol = volume_control.lock().await;
                                  vol.muted = true;
                                  drop(vol);
                                  let vol2 = volume_control.lock().await;
                                  let _ = vol2.apply_mute(&mut child).await;
                                  drop(vol2);
                              }
                              ui_state.station_index = station_index;
                              draw_ui(&mut terminal, &ui_state, STATIONS);
                          }
                         KeyCode::F(8) => {
                             // Play/Pause as mute toggle
                             let mut vol = volume_control.lock().await;
                             vol.toggle_mute();
                             let target_vol = vol.volume;
                             let player_type = vol.player_type;
                             drop(vol);

                              let vol = volume_control.lock().await;
                              let needs_restart = vol.apply_mute(&mut child).await.is_err();
                              let _player_type = vol.player_type;
                              drop(vol);

                             if needs_restart {
                                 let _ = child.start_kill();
                                 let _ = tokio::time::timeout(
                                     tokio::time::Duration::from_millis(500),
                                     child.wait()
                                 ).await;
                                  let (cmd, args, new_socket) = build_player_args(player_type, stream_url, target_vol);
                                 if let Some(s) = new_socket {
                                     let mut vol = volume_control.lock().await;
                                     vol.mpv_socket = Some(s);
                                     drop(vol);
                                 }
                                 child = TokioCommand::new(&cmd)
                                     .args(&args)
                                     .stdout(Stdio::null())
                                     .stderr(Stdio::null())
                                     .spawn()?;
                             }

                             let vol = volume_control.lock().await;
                             ui_state.volume = vol.volume;
                             ui_state.muted = vol.muted;
                             drop(vol);
                             draw_ui(&mut terminal, &ui_state, STATIONS);
                         }
                         KeyCode::F(12) | KeyCode::Char('m') | KeyCode::Char('M') => {
                             // Toggle mute flag and target volume
                             let mut vol = volume_control.lock().await;
                             vol.toggle_mute();
                             let target_vol = vol.volume; // 0 when muted, previous volume when unmuted
                             let player_type = vol.player_type;
                             drop(vol);

                             // For ffplay and other players without reliable runtime mute, always restart with target_vol
                             let _ = child.start_kill();
                             let _ = tokio::time::timeout(
                                 tokio::time::Duration::from_millis(500),
                                 child.wait()
                             ).await;
                             let (cmd, args, new_socket) = build_player_args(player_type, stream_url, target_vol);
                             if let Some(s) = new_socket {
                                 let mut vol = volume_control.lock().await;
                                 vol.mpv_socket = Some(s);
                                 drop(vol);
                             }
                             child = TokioCommand::new(&cmd)
                                 .args(&args)
                                 .stdout(Stdio::null())
                                 .stderr(Stdio::null())
                                 .spawn()?;

                             // Update UI
                             let vol = volume_control.lock().await;
                             ui_state.volume = vol.volume;
                             ui_state.muted = vol.muted;
                             drop(vol);
                             draw_ui(&mut terminal, &ui_state, STATIONS);
                         }
                        KeyCode::Char('q') | KeyCode::Char('Q') => {
                            println!("\n\nStopping...");
                            let _ = child.start_kill();
                            let _ = tokio::time::timeout(
                                tokio::time::Duration::from_millis(500),
                                child.wait()
                            ).await;
                            break;
                        }
                        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                            println!("\n\nStopping (Ctrl+C)...");
                            let _ = child.start_kill();
                            let _ = tokio::time::timeout(
                                tokio::time::Duration::from_millis(500),
                                child.wait()
                            ).await;
                            break;
                        }
                        _ => {}
                    }
                }
            }
            // Update elapsed time
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
                let elapsed = start_time.elapsed();
                ui_state.elapsed = elapsed;
                draw_ui(&mut terminal, &ui_state, STATIONS);
            }
        }
        
        #[cfg(not(unix))]
        tokio::select! {
            // Check if the process has finished
            result = child.wait() => {
                match result {
                    Ok(status) => {
                        if status.success() {
                            println!("\n\nPlayback finished");
                        } else {
                            println!("\n\nPlayback stopped");
                        }
                        break;
                    }
                    Err(e) => {
                        eprintln!("\n\nError: {}", e);
                        break;
                    }
                }
            }
            // Handle keyboard input
            key_result = tokio::task::spawn_blocking(|| {
                if event::poll(std::time::Duration::from_millis(100)).unwrap_or(false) {
                    if let Ok(Event::Key(KeyEvent { code, kind, .. })) = event::read() {
                        if kind == KeyEventKind::Press {
                            return Some(code);
                        }
                    }
                }
                None
            }) => {
                if let Ok(Some(key_code)) = key_result {
                    match key_code {
                        KeyCode::F(11) | KeyCode::Up => {
                            let mut vol = volume_control.lock().await;
                            if !vol.muted {
                                vol.increase_volume();
                            }
                            let current_vol = vol.volume;
                            drop(vol);

                            let vol = volume_control.lock().await;
                            let needs_restart = vol.apply_volume(&mut child).await.is_err();
                            let player_type = vol.player_type;
                            drop(vol);

                            if needs_restart {
                                let _ = child.start_kill();
                                let _ = tokio::time::timeout(
                                    tokio::time::Duration::from_millis(500),
                                    child.wait()
                                ).await;
                                let (cmd, args, new_socket) = build_player_args(player_type, stream_url, current_vol);
                                if let Some(s) = new_socket {
                                    let mut vol = volume_control.lock().await;
                                    vol.mpv_socket = Some(s);
                                    drop(vol);
                                }
                                child = TokioCommand::new(&cmd)
                                    .args(&args)
                                    .stdout(Stdio::null())
                                    .stderr(Stdio::null())
                                    .spawn()?;
                            }
                            // Update UI
                            let vol = volume_control.lock().await;
                            ui_state.volume = vol.volume;
                            ui_state.muted = vol.muted;
                            drop(vol);
                            draw_ui(&mut terminal, &ui_state, STATIONS);
                        }
                        KeyCode::F(10) | KeyCode::Down => {
                            let mut vol = volume_control.lock().await;
                            if !vol.muted {
                                vol.decrease_volume();
                            }
                            let current_vol = vol.volume;
                            drop(vol);
                            
                            let vol = volume_control.lock().await;
                            let needs_restart = vol.apply_volume(&mut child).await.is_err();
                            let player_type = vol.player_type;
                            drop(vol);
                            
                            if needs_restart {
                                let _ = child.start_kill();
                                let _ = tokio::time::timeout(
                                    tokio::time::Duration::from_millis(500),
                                    child.wait()
                                ).await;
                                let (cmd, args, new_socket) = build_player_args(player_type, stream_url, current_vol);
                                if let Some(s) = new_socket {
                                    let mut vol = volume_control.lock().await;
                                    vol.mpv_socket = Some(s);
                                    drop(vol);
                                }
                                child = TokioCommand::new(&cmd)
                                    .args(&args)
                                    .stdout(Stdio::null())
                                    .stderr(Stdio::null())
                                    .spawn()?;
                            }
                            // Update UI
                            let vol = volume_control.lock().await;
                            ui_state.volume = vol.volume;
                            ui_state.muted = vol.muted;
                            drop(vol);
                            draw_ui(&mut terminal, &ui_state, STATIONS);
                        }
                        KeyCode::F(7) | KeyCode::Left => {
                            let mut vol = volume_control.lock().await;
                            let current_vol = vol.volume;
                            let is_muted = vol.muted;
                            let player_type = vol.player_type;
                            drop(vol);

                            station_index = if station_index == 0 { STATIONS.len() - 1 } else { station_index - 1 };
                            stream_url = STATIONS[station_index].url;

                            let _ = child.start_kill();
                            let _ = tokio::time::timeout(
                                tokio::time::Duration::from_millis(500),
                                child.wait()
                            ).await;
                            let (cmd, args, new_socket) = build_player_args(player_type, stream_url, current_vol);
                            if let Some(s) = new_socket {
                                let mut vol = volume_control.lock().await;
                                vol.mpv_socket = Some(s);
                                drop(vol);
                            }
                            child = TokioCommand::new(&cmd)
                                .args(&args)
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .spawn()?;

                            if is_muted {
                                let mut vol = volume_control.lock().await;
                                vol.muted = true;
                                drop(vol);
                                let vol2 = volume_control.lock().await;
                                let _ = vol2.apply_mute(&mut child).await;
                                drop(vol2);
                            }
                            ui_state.station_index = station_index;
                            draw_ui(&mut terminal, &ui_state, STATIONS);
                        }
                        KeyCode::F(9) | KeyCode::Right => {
                            let mut vol = volume_control.lock().await;
                            let current_vol = vol.volume;
                            let is_muted = vol.muted;
                            let player_type = vol.player_type;
                            drop(vol);

                            station_index = (station_index + 1) % STATIONS.len();
                            stream_url = STATIONS[station_index].url;

                            let _ = child.start_kill();
                            let _ = tokio::time::timeout(
                                tokio::time::Duration::from_millis(500),
                                child.wait()
                            ).await;
                            let (cmd, args, new_socket) = build_player_args(player_type, stream_url, current_vol);
                            if let Some(s) = new_socket {
                                let mut vol = volume_control.lock().await;
                                vol.mpv_socket = Some(s);
                                drop(vol);
                            }
                            child = TokioCommand::new(&cmd)
                                .args(&args)
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .spawn()?;

                            if is_muted {
                                let mut vol = volume_control.lock().await;
                                vol.muted = true;
                                drop(vol);
                                let vol2 = volume_control.lock().await;
                                let _ = vol2.apply_mute(&mut child).await;
                                drop(vol2);
                            }
                            ui_state.station_index = station_index;
                            draw_ui(&mut terminal, &ui_state, STATIONS);
                        }
                        KeyCode::F(8) | KeyCode::F(12) | KeyCode::Char('m') | KeyCode::Char('M') => {
                            let mut vol = volume_control.lock().await;
                            vol.toggle_mute();
                            let current_vol = vol.volume;
                            let player_type = vol.player_type;
                            drop(vol);
                            
                            let vol = volume_control.lock().await;
                            let needs_restart = vol.apply_mute(&mut child).await.is_err();
                            let player_type2 = vol.player_type;
                            drop(vol);
                            
                            if needs_restart {
                                let _ = child.start_kill();
                                let _ = tokio::time::timeout(
                                    tokio::time::Duration::from_millis(500),
                                    child.wait()
                                ).await;
                                let (cmd, args, new_socket) = build_player_args(player_type2, stream_url, current_vol);
                                if let Some(s) = new_socket {
                                    let mut vol = volume_control.lock().await;
                                    vol.mpv_socket = Some(s);
                                    drop(vol);
                                }
                            child = TokioCommand::new(&cmd)
                                .args(&args)
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .spawn()?;
                        }
                        // Update UI
                        let vol = volume_control.lock().await;
                        ui_state.volume = vol.volume;
                        ui_state.muted = vol.muted;
                        drop(vol);
                        draw_ui(&mut terminal, &ui_state, STATIONS);
                        }
                        KeyCode::Char('q') | KeyCode::Char('Q') => {
                            println!("\n\nStopping...");
                            let _ = child.start_kill();
                            let _ = tokio::time::timeout(
                                tokio::time::Duration::from_millis(500),
                                child.wait()
                            ).await;
                            break;
                        }
                        _ => {}
                    }
                }
            }
            // Update elapsed time
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
                let elapsed = start_time.elapsed();
                ui_state.elapsed = elapsed;
                draw_ui(&mut terminal, &ui_state, STATIONS);
            }
        }
    }

    // Restore terminal
    terminal.clear()?;
    disable_raw_mode()?;
    // Show cursor back
    {
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, Show);
    }
    println!();
    
    Ok(())
}
