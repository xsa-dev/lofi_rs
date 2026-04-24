mod player;
mod ui;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::{
    cursor::{Hide, Show, MoveTo},
    execute,
};
use ratatui::{backend::CrosstermBackend, Terminal};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::player::{
    build_player_args, detect_player, spawn_player, PlayerType, VolumeControl,
};
use crate::ui::{draw_ui, UiState, STATIONS};

// ─── Metadata ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct NpResponse {
    now_playing: NpInner,
}

#[derive(Deserialize)]
struct NpInner {
    song: NpSong,
}

#[derive(Deserialize)]
struct NpSong {
    title: String,
    artist: String,
}

async fn fetch_now_playing(url: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let resp = client.get(url).send().await.ok()?;
    let np: NpResponse = resp.json().await.ok()?;
    let artist = np.now_playing.song.artist.trim();
    let title = np.now_playing.song.title.trim();
    if artist.is_empty() && title.is_empty() {
        None
    } else if artist.is_empty() {
        Some(title.to_string())
    } else {
        Some(format!("{} — {}", artist, title))
    }
}

// ─── Player helpers ───────────────────────────────────────────────────────────

/// Kill the current child and spawn a fresh one for `stream_url` at `volume`.
/// Updates the IPC socket in `volume_control` if needed.
async fn restart_player(
    child: &mut tokio::process::Child,
    volume_control: &Arc<Mutex<VolumeControl>>,
    stream_url: &str,
    volume: u32,
) -> Result<tokio::process::Child, Box<dyn std::error::Error>> {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;

    let player_type = volume_control.lock().await.player_type;
    let (cmd, args, new_socket) = build_player_args(player_type, stream_url, volume);

    if let Some(s) = new_socket {
        volume_control.lock().await.mpv_socket = Some(s);
    }

    Ok(spawn_player(&cmd, &args).await?)
}

// ─── Key handling ─────────────────────────────────────────────────────────────

/// Poll for a single key-press event (non-blocking, 100 ms timeout).
/// Returns `Some((KeyCode, KeyModifiers))` on press, `None` otherwise.
fn poll_key() -> Option<(KeyCode, KeyModifiers)> {
    if event::poll(Duration::from_millis(100)).unwrap_or(false) {
        if let Ok(Event::Key(KeyEvent {
            code,
            kind: KeyEventKind::Press,
            modifiers,
            ..
        })) = event::read()
        {
            return Some((code, modifiers));
        }
    }
    None
}

// ─── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut station_index: usize = 0;
    let mut stream_url: &str = STATIONS[station_index].url;
    let mut ui_state = UiState::new();
    ui_state.station_index = station_index;

    // Detect available player: prefer mpv → ffplay → afplay+curl
    let player_type = match detect_player() {
        Some(p) => p,
        None => {
            eprintln!("Error: No suitable player found");
            eprintln!("Please install one of the following:");
            if cfg!(target_os = "macos") {
                eprintln!("  macOS: brew install ffmpeg or brew install mpv");
            } else {
                eprintln!("  Linux: sudo apt-get install ffmpeg or sudo apt-get install mpv");
            }
            return Err("Player not found".into());
        }
    };

    let mut volume_control = VolumeControl::new(player_type);

    let (player_cmd, player_args, socket_path) =
        build_player_args(player_type, stream_url, volume_control.volume);

    if let Some(socket) = socket_path {
        volume_control.mpv_socket = Some(socket);
    }

    let volume_control = Arc::new(Mutex::new(volume_control));

    // Set up terminal
    enable_raw_mode()?;
    {
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, Hide, Clear(ClearType::All), MoveTo(0, 0));
    }
    let stdout = std::io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Spawn player
    let mut child = spawn_player(&player_cmd, &player_args).await?;

    // Initial UI render
    {
        let vol = volume_control.lock().await;
        ui_state.volume = vol.volume;
        ui_state.muted = vol.muted;
    }
    draw_ui(&mut terminal, &ui_state, STATIONS);

    let start_time = Instant::now();

    // Ctrl+C signal (unix only)
    #[cfg(unix)]
    let mut ctrl_c =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    // Now-playing background poller
    let now_playing_state: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let track_changed = Arc::new(tokio::sync::Notify::new());
    let (md_tx, md_rx) =
        tokio::sync::watch::channel::<Option<&'static str>>(STATIONS[station_index].metadata_url);
    {
        let np = now_playing_state.clone();
        let tc = track_changed.clone();
        let mut rx = md_rx;
        tokio::spawn(async move {
            let mut last_track: Option<String> = None;
            loop {
                let url = *rx.borrow();
                let result = if let Some(u) = url {
                    fetch_now_playing(u).await
                } else {
                    None
                };
                if let (Some(prev), Some(new)) = (&last_track, &result) {
                    if prev != new {
                        tc.notify_one();
                    }
                }
                last_track = result.clone();
                *np.lock().await = result;
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(15)) => {}
                    _ = rx.changed() => { last_track = None; }
                }
            }
        });
    }

    // 1 Hz UI ticker
    let mut ui_tick = tokio::time::interval(Duration::from_secs(1));
    ui_tick.tick().await; // consume immediate first tick

    // ─── Event loop ──────────────────────────────────────────────────────────
    loop {
        // Shared select arms (platform-independent)
        let key_future = tokio::task::spawn_blocking(poll_key);

        // Platform-specific Ctrl+C arm is handled via cfg blocks below.
        // All other arms are identical — we use a macro-free approach by
        // extracting the common logic into helper closures/functions above.

        enum Event_ {
            TrackChanged,
            ChildExited,
            Key(KeyCode, KeyModifiers),
            Tick,
            #[cfg(unix)]
            CtrlC,
        }

        let event = {
            #[cfg(unix)]
            {
                tokio::select! {
                    _ = track_changed.notified(), if matches!(player_type, PlayerType::Ffplay) => Event_::TrackChanged,
                    _ = child.wait() => Event_::ChildExited,
                    _ = ctrl_c.recv() => Event_::CtrlC,
                    res = key_future => {
                        if let Ok(Some((code, mods))) = res { Event_::Key(code, mods) } else { continue }
                    }
                    _ = ui_tick.tick() => Event_::Tick,
                }
            }
            #[cfg(not(unix))]
            {
                tokio::select! {
                    _ = track_changed.notified(), if matches!(player_type, PlayerType::Ffplay) => Event_::TrackChanged,
                    _ = child.wait() => Event_::ChildExited,
                    res = key_future => {
                        if let Ok(Some((code, mods))) = res { Event_::Key(code, mods) } else { continue }
                    }
                    _ = ui_tick.tick() => Event_::Tick,
                }
            }
        };

        match event {
            // ── ffplay track-boundary workaround ──────────────────────────
            Event_::TrackChanged => {
                let vol = volume_control.lock().await.volume;
                child = restart_player(&mut child, &volume_control, stream_url, vol).await?;
            }

            // ── child exited unexpectedly ─────────────────────────────────
            Event_::ChildExited => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let vol = volume_control.lock().await.volume;
                child = restart_player(&mut child, &volume_control, stream_url, vol).await?;
            }

            // ── Ctrl+C (unix) ─────────────────────────────────────────────
            #[cfg(unix)]
            Event_::CtrlC => {
                let _ = child.kill().await;
                break;
            }

            // ── 1-second UI tick ──────────────────────────────────────────
            Event_::Tick => {
                ui_state.elapsed = start_time.elapsed();
                ui_state.now_playing = now_playing_state.lock().await.clone();
                draw_ui(&mut terminal, &ui_state, STATIONS);
            }

            // ── Keyboard ──────────────────────────────────────────────────
            Event_::Key(key_code, modifiers) => {
                match key_code {
                    // Volume up
                    KeyCode::F(11) | KeyCode::Up => {
                        let vol = {
                            let mut vc = volume_control.lock().await;
                            if !vc.muted {
                                vc.increase_volume();
                            }
                            vc.volume
                        };
                        let needs_restart = volume_control
                            .lock()
                            .await
                            .apply_volume(&mut child)
                            .await
                            .is_err();
                        if needs_restart {
                            child =
                                restart_player(&mut child, &volume_control, stream_url, vol)
                                    .await?;
                        }
                        let vc = volume_control.lock().await;
                        ui_state.volume = vc.volume;
                        ui_state.muted = vc.muted;
                        draw_ui(&mut terminal, &ui_state, STATIONS);
                    }

                    // Volume down
                    KeyCode::F(10) | KeyCode::Down => {
                        let vol = {
                            let mut vc = volume_control.lock().await;
                            if !vc.muted {
                                vc.decrease_volume();
                            }
                            vc.volume
                        };
                        let needs_restart = volume_control
                            .lock()
                            .await
                            .apply_volume(&mut child)
                            .await
                            .is_err();
                        if needs_restart {
                            child =
                                restart_player(&mut child, &volume_control, stream_url, vol)
                                    .await?;
                        }
                        let vc = volume_control.lock().await;
                        ui_state.volume = vc.volume;
                        ui_state.muted = vc.muted;
                        draw_ui(&mut terminal, &ui_state, STATIONS);
                    }

                    // Previous station
                    KeyCode::F(7) | KeyCode::Left => {
                        let (vol, is_muted) = {
                            let vc = volume_control.lock().await;
                            (vc.volume, vc.muted)
                        };
                        station_index = if station_index == 0 {
                            STATIONS.len() - 1
                        } else {
                            station_index - 1
                        };
                        stream_url = STATIONS[station_index].url;
                        let _ = md_tx.send(STATIONS[station_index].metadata_url);
                        *now_playing_state.lock().await = None;

                        child =
                            restart_player(&mut child, &volume_control, stream_url, vol).await?;

                        if is_muted {
                            volume_control.lock().await.muted = true;
                            let _ = volume_control
                                .lock()
                                .await
                                .apply_mute(&mut child)
                                .await;
                        }
                        ui_state.station_index = station_index;
                        draw_ui(&mut terminal, &ui_state, STATIONS);
                    }

                    // Next station
                    KeyCode::F(9) | KeyCode::Right => {
                        let (vol, is_muted) = {
                            let vc = volume_control.lock().await;
                            (vc.volume, vc.muted)
                        };
                        station_index = (station_index + 1) % STATIONS.len();
                        stream_url = STATIONS[station_index].url;
                        let _ = md_tx.send(STATIONS[station_index].metadata_url);
                        *now_playing_state.lock().await = None;

                        child =
                            restart_player(&mut child, &volume_control, stream_url, vol).await?;

                        if is_muted {
                            volume_control.lock().await.muted = true;
                            let _ = volume_control
                                .lock()
                                .await
                                .apply_mute(&mut child)
                                .await;
                        }
                        ui_state.station_index = station_index;
                        draw_ui(&mut terminal, &ui_state, STATIONS);
                    }

                    // Play/Pause (mute toggle via F8)
                    KeyCode::F(8) => {
                        let target_vol = {
                            let mut vc = volume_control.lock().await;
                            vc.toggle_mute();
                            vc.volume
                        };
                        let needs_restart = volume_control
                            .lock()
                            .await
                            .apply_mute(&mut child)
                            .await
                            .is_err();
                        if needs_restart {
                            child = restart_player(
                                &mut child,
                                &volume_control,
                                stream_url,
                                target_vol,
                            )
                            .await?;
                        }
                        let vc = volume_control.lock().await;
                        ui_state.volume = vc.volume;
                        ui_state.muted = vc.muted;
                        draw_ui(&mut terminal, &ui_state, STATIONS);
                    }

                    // Mute toggle (F12 / m / M)
                    KeyCode::F(12) | KeyCode::Char('m') | KeyCode::Char('M') => {
                        let target_vol = {
                            let mut vc = volume_control.lock().await;
                            vc.toggle_mute();
                            vc.volume
                        };
                        // Always restart to guarantee mute takes effect
                        child = restart_player(
                            &mut child,
                            &volume_control,
                            stream_url,
                            target_vol,
                        )
                        .await?;
                        let vc = volume_control.lock().await;
                        ui_state.volume = vc.volume;
                        ui_state.muted = vc.muted;
                        draw_ui(&mut terminal, &ui_state, STATIONS);
                    }

                    // Quit
                    KeyCode::Char('q') | KeyCode::Char('Q') => {
                        let _ = child.start_kill();
                        let _ = tokio::time::timeout(
                            Duration::from_millis(500),
                            child.wait(),
                        )
                        .await;
                        break;
                    }

                    // Ctrl+C (non-unix fallback via keyboard)
                    KeyCode::Char('c')
                        if modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        let _ = child.start_kill();
                        let _ = tokio::time::timeout(
                            Duration::from_millis(500),
                            child.wait(),
                        )
                        .await;
                        break;
                    }

                    _ => {}
                }
            }
        }
    }

    // Restore terminal
    terminal.clear()?;
    disable_raw_mode()?;
    {
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, Show);
    }
    println!();

    Ok(())
}
