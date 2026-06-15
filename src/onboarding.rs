//! First-run setup.
//!
//! Two stages, sharing the TUI's aesthetic (white accent, dim everything else,
//! transparent background): pick a provider preset, then fill in the API key
//! and confirm the model / endpoint / working directory. The result is written
//! to `~/.wizard/config.toml` with the key stored inline so the agent can
//! authenticate without any extra environment setup.

use std::path::PathBuf;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::auth::xai_oauth;
use crate::config::{Auth, Config};

const ACCENT: Color = Color::White;
const DIM: Color = Color::DarkGray;
const TEXT_DIM: Color = Color::Gray;

fn dim() -> Style {
    Style::default().fg(DIM)
}

fn accent() -> Style {
    Style::default().fg(ACCENT)
}

/// A provider preset: a name plus the endpoint defaults it prefills.
struct Provider {
    name: &'static str,
    base_url: &'static str,
    api_type: &'static str,
    /// Default model tag (empty when there is no sensible default).
    model: &'static str,
    /// Local endpoints need no API key.
    local: bool,
    /// Sign in with xAI OAuth instead of entering a key.
    oauth: bool,
}

const PROVIDERS: &[Provider] = &[
    Provider {
        name: "xAI (sign in with your account)",
        base_url: xai_oauth::DEFAULT_BASE_URL,
        api_type: "openai_chat_completion",
        model: xai_oauth::DEFAULT_MODEL,
        local: false,
        oauth: true,
    },
    Provider {
        name: "xAI (Grok) — API key",
        base_url: "https://api.x.ai/v1",
        api_type: "openai_chat_completion",
        model: "grok-4.3",
        local: false,
        oauth: false,
    },
    Provider {
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        api_type: "openai_chat_completion",
        model: "gpt-4o",
        local: false,
        oauth: false,
    },
    Provider {
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        api_type: "openai_chat_completion",
        model: "",
        local: false,
        oauth: false,
    },
    Provider {
        name: "Local (llama.cpp / Ollama)",
        base_url: "http://127.0.0.1:8080/v1",
        api_type: "openai_chat_completion",
        model: "",
        local: true,
        oauth: false,
    },
    Provider {
        name: "Custom (OpenAI-compatible)",
        base_url: "",
        api_type: "openai_chat_completion",
        model: "",
        local: false,
        oauth: false,
    },
];

/// Field indices in the second-stage form.
const BASE_URL: usize = 0;
const API_TYPE: usize = 1;
const MODEL: usize = 2;
const API_KEY: usize = 3;
const WORKDIR: usize = 4;

struct Field {
    label: &'static str,
    hint: &'static str,
    value: String,
}

impl Field {
    fn new(label: &'static str, hint: &'static str, value: impl Into<String>) -> Self {
        Self {
            label,
            hint,
            value: value.into(),
        }
    }
}

enum Stage {
    Provider {
        selected: usize,
    },
    Fields {
        fields: Vec<Field>,
        selected: usize,
        local: bool,
    },
}

/// What the form loop resolved to once the terminal is restored.
enum Outcome {
    /// A static-key (or local) config, ready to save.
    Static(Box<Config>),
    /// The user chose xAI sign-in: run the OAuth flow, then save.
    Oauth,
}

/// Run first-run setup. Returns the new [`Config`], or `None` when the user
/// cancels (Esc on the provider screen). Requires an interactive terminal.
pub async fn run() -> Result<Option<Config>> {
    let mut stage = Stage::Provider { selected: 0 };

    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;

    let result = loop {
        match &mut stage {
            Stage::Provider { selected } => {
                terminal.draw(|frame| draw_provider(frame, *selected))?;
            }
            Stage::Fields {
                fields, selected, ..
            } => {
                terminal.draw(|frame| draw_fields(frame, fields, *selected))?;
            }
        }

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        let ctrl_quit = key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'));
        if ctrl_quit {
            break None;
        }

        match &mut stage {
            Stage::Provider { selected } => match key.code {
                KeyCode::Esc => break None,
                KeyCode::Up | KeyCode::BackTab => {
                    *selected = selected.checked_sub(1).unwrap_or(PROVIDERS.len() - 1);
                }
                KeyCode::Down | KeyCode::Tab => {
                    *selected = (*selected + 1) % PROVIDERS.len();
                }
                KeyCode::Enter => {
                    let provider = &PROVIDERS[*selected];
                    if provider.oauth {
                        // Sign-in needs the real terminal (browser + prompts),
                        // so leave the form and run it after teardown.
                        break Some(Outcome::Oauth);
                    }
                    stage = Stage::Fields {
                        fields: fields_for(provider),
                        selected: first_empty_field(provider),
                        local: provider.local,
                    };
                }
                _ => {}
            },
            Stage::Fields {
                fields,
                selected,
                local,
            } => match key.code {
                // Esc returns to the provider menu so a mis-pick is cheap.
                KeyCode::Esc => stage = Stage::Provider { selected: 0 },
                KeyCode::Up | KeyCode::BackTab => {
                    *selected = selected.checked_sub(1).unwrap_or(fields.len() - 1);
                }
                KeyCode::Down | KeyCode::Tab => {
                    *selected = (*selected + 1) % fields.len();
                }
                KeyCode::Enter => {
                    if *selected + 1 < fields.len() {
                        *selected += 1;
                    } else if form_ready(fields, *local) {
                        break Some(Outcome::Static(Box::new(build_config(fields, *local))));
                    }
                }
                KeyCode::Backspace => {
                    fields[*selected].value.pop();
                }
                KeyCode::Char(c) => {
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        fields[*selected].value.push(c);
                    }
                }
                _ => {}
            },
        }
    };

    drop(_guard);
    restore_terminal()?;

    let config = match result {
        None => return Ok(None),
        Some(Outcome::Static(config)) => *config,
        Some(Outcome::Oauth) => oauth_config().await?,
    };
    config.save().context("saving the new config")?;
    Ok(Some(config))
}

/// Run the xAI sign-in (browser flow on the now-restored terminal) and return
/// the matching OAuth config.
async fn oauth_config() -> Result<Config> {
    println!("\n✦ signing in to xAI…\n");
    xai_oauth::login(|line| println!("{line}")).await?;
    Ok(Config {
        base_url: xai_oauth::DEFAULT_BASE_URL.to_string(),
        api_type: "openai_chat_completion".to_string(),
        model: xai_oauth::DEFAULT_MODEL.to_string(),
        auth: Auth::XaiOauth,
        api_key: None,
        api_key_env: None,
        workdir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        ..Config::default()
    })
}

/// The form fields for a chosen provider, prefilled from its preset.
fn fields_for(provider: &Provider) -> Vec<Field> {
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .display()
        .to_string();
    vec![
        Field::new("base url", "OpenAI-compatible endpoint", provider.base_url),
        Field::new(
            "api type",
            "openai_chat_completion · openai_responses · anthropic_chat_completion",
            provider.api_type,
        ),
        Field::new("model", "model tag", provider.model),
        Field::new(
            "api key",
            if provider.local {
                "leave blank for a local server"
            } else {
                "paste your key (stored in ~/.wizard/config.toml)"
            },
            "",
        ),
        Field::new("working dir", "directory the agent operates in", cwd),
    ]
}

/// Start the cursor on the first field the user still needs to fill.
fn first_empty_field(provider: &Provider) -> usize {
    if provider.base_url.is_empty() {
        BASE_URL
    } else if provider.model.is_empty() {
        MODEL
    } else if provider.local {
        WORKDIR
    } else {
        API_KEY
    }
}

/// All required fields present (the API key is optional only for local servers).
fn form_ready(fields: &[Field], local: bool) -> bool {
    let filled = |i: usize| !fields[i].value.trim().is_empty();
    filled(BASE_URL)
        && filled(API_TYPE)
        && filled(MODEL)
        && filled(WORKDIR)
        && (local || filled(API_KEY))
}

fn build_config(fields: &[Field], local: bool) -> Config {
    let key = fields[API_KEY].value.trim();
    // Local servers still want a non-empty token for the OpenAI client.
    let api_key = if key.is_empty() {
        local.then(|| "local".to_string())
    } else {
        Some(key.to_string())
    };
    Config {
        base_url: fields[BASE_URL].value.trim().to_string(),
        api_type: fields[API_TYPE].value.trim().to_string(),
        model: fields[MODEL].value.trim().to_string(),
        api_key,
        api_key_env: None,
        workdir: PathBuf::from(shellexpand::tilde(fields[WORKDIR].value.trim()).into_owned()),
        ..Config::default()
    }
}

fn draw_provider(frame: &mut ratatui::Frame, selected: usize) {
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled("✦  wizard setup", accent().bold())),
        Line::from(Span::styled(
            "choose a provider  ·  ↑↓ move · Enter select · Esc cancel",
            dim().italic(),
        )),
        Line::raw(""),
    ];
    for (index, provider) in PROVIDERS.iter().enumerate() {
        let active = index == selected;
        let marker = if active { "❯ " } else { "  " };
        let style = if active {
            accent().bold()
        } else {
            Style::default().fg(TEXT_DIM)
        };
        lines.push(Line::from(vec![
            Span::styled(marker, accent()),
            Span::styled(provider.name, style),
        ]));
    }
    render_card(frame, lines);
}

fn draw_fields(frame: &mut ratatui::Frame, fields: &[Field], selected: usize) {
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled("✦  wizard setup", accent().bold())),
        Line::from(Span::styled(
            "Tab/↑↓ move · type to edit · Enter next · Esc back",
            dim().italic(),
        )),
        Line::raw(""),
    ];

    for (index, field) in fields.iter().enumerate() {
        let active = index == selected;
        let marker = if active { "❯ " } else { "  " };
        let label_style = if active {
            accent().bold()
        } else {
            Style::default().fg(TEXT_DIM)
        };
        lines.push(Line::from(vec![
            Span::styled(marker, accent()),
            Span::styled(format!("{:<12}", field.label), label_style),
            Span::styled(field.hint.to_string(), dim().italic()),
        ]));
        // Mask the API key so it is not shouted onto the screen.
        let shown = if index == API_KEY {
            "•".repeat(field.value.chars().count())
        } else {
            field.value.clone()
        };
        let cursor = if active { "▍" } else { "" };
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(shown, Style::default().fg(Color::White)),
            Span::styled(cursor, dim()),
        ]));
        lines.push(Line::raw(""));
    }

    render_card(frame, lines);
}

/// Render a centered, left-aligned card of `lines`.
fn render_card(frame: &mut ratatui::Frame, lines: Vec<Line<'static>>) {
    let area = frame.area();
    let height = lines.len() as u16;
    let width = area.width.saturating_sub(4).min(76);
    let card = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height: height.min(area.height),
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Left),
        card,
    );
}

// --- terminal lifecycle (mirrors crate::app, kept local to onboarding) ---

type Tui = Terminal<CrosstermBackend<std::io::Stdout>>;

fn setup_terminal() -> Result<Tui> {
    crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)
        .context("entering alternate screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("creating terminal")
}

fn restore_terminal() -> Result<()> {
    crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen)
        .context("leaving alternate screen")?;
    crossterm::terminal::disable_raw_mode().context("disabling raw mode")?;
    Ok(())
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
            let _ = restore_terminal();
        }
    }
}
