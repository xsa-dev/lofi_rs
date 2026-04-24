use ratatui::{
    backend::Backend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Terminal,
};
use std::time::Duration;

#[derive(Clone)]
pub struct Station {
    pub name: &'static str,
    pub url: &'static str,
    pub metadata_url: Option<&'static str>,
}

pub const STATIONS: &[Station] = &[
    Station {
        name: "Lofi 1",
        url: "https://stream.zeno.fm/0r0xa792kwzuv",
        metadata_url: None,
    },
    Station {
        name: "Lofi 2",
        url: "https://stream.zeno.fm/v5reddyk8rhvv",
        metadata_url: None,
    },
    Station {
        name: "Code Radio",
        url: "https://coderadio-admin-v2.freecodecamp.org/listen/coderadio/radio.mp3",
        metadata_url: Some("https://coderadio-admin-v2.freecodecamp.org/api/nowplaying/coderadio"),
    },
];

pub struct UiState {
    pub station_index: usize,
    pub volume: u32,
    pub muted: bool,
    pub elapsed: Duration,
    pub now_playing: Option<String>,
}

impl UiState {
    pub fn new() -> Self {
        Self {
            station_index: 0,
            volume: 70,
            muted: false,
            elapsed: Duration::ZERO,
            now_playing: None,
        }
    }
}

pub fn draw_ui<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &UiState,
    stations: &[Station],
) {
    terminal
        .draw(|f| {
            let size = f.size();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(stations.len() as u16 + 2),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Min(0),
                ])
                .split(size);

            // Stations list
            let items: Vec<ListItem> = stations
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let style = if i == state.station_index {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default()
                    };
                    ListItem::new(format!(
                        "{} {}",
                        if i == state.station_index { "->" } else { "  " },
                        s.name
                    ))
                    .style(style)
                })
                .collect();
            let list = List::new(items).block(Block::default().borders(Borders::ALL).title("Stations"));
            f.render_widget(list, chunks[0]);

            // Status
            let hours = state.elapsed.as_secs() / 3600;
            let minutes = (state.elapsed.as_secs() % 3600) / 60;
            let seconds = state.elapsed.as_secs() % 60;
            let bar_len = 30;
            let filled = if state.muted {
                0
            } else {
                (state.volume * bar_len) / 100
            };
            let bar = format!(
                "[{}{}]",
                "#".repeat(filled as usize),
                "-".repeat((bar_len - filled) as usize)
            );
            let mute_status = if state.muted { " [MUTED]" } else { "" };
            let status_text = format!(
                "Elapsed: {:02}:{:02}:{:02} | Volume: {:>3}% {}{}",
                hours, minutes, seconds, state.volume, bar, mute_status
            );
            let status = Paragraph::new(status_text)
                .block(Block::default().borders(Borders::ALL).title("Status"));
            f.render_widget(status, chunks[1]);

            // Now Playing
            let has_meta = stations
                .get(state.station_index)
                .and_then(|s| s.metadata_url)
                .is_some();
            let np_text: &str = match state.now_playing.as_deref() {
                Some(s) => s,
                None if has_meta => "Loading...",
                None => "—",
            };
            let now_playing = Paragraph::new(np_text)
                .block(Block::default().borders(Borders::ALL).title("Now Playing"));
            f.render_widget(now_playing, chunks[2]);

            // Controls
            let controls_text = "Controls:\nF11: Vol Up | F10: Vol Down | F12: Mute\nF7: Prev Station | F9: Next Station | F8: Play/Pause\nq: Quit";
            let controls = Paragraph::new(controls_text)
                .block(Block::default().borders(Borders::ALL).title("Controls"));
            f.render_widget(controls, chunks[3]);
        })
        .unwrap();
}
