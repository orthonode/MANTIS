#![allow(dead_code)]
//! Premium 6-tab MANTIS dashboard — Bloomberg-style dark theme.
//!
//! Design language (research-validated against Bloomberg Terminal, btop, CLOBster):
//!   Background  #0D0D0D  near-black — never pure black, reduces eye strain
//!   Borders     #2D2D45  very subtle — panels breathe, not caged
//!   Text        #D2D2DC  silver — not blinding white
//!   Green       #00D97E  profit / OK / connected
//!   Red         #FF4444  loss / error / disconnected
//!   Amber       #FFA028  Bloomberg Sunshade — ONLY for actionable signals
//!   Blue        #00BFFF  Binance feed
//!   Purple      #9B59B6  Chainlink oracle feed
//!   Cyan        #4AF6C3  neutral market data
//!   Slate       #7B68EE  active tab / selected row
//!
//! Rules (Bloomberg-validated):
//!   Labels are always dimmer than their values
//!   Red is ONLY for losses and errors — never branding
//!   Amber is ONLY for actionable signals — never decoration
//!   Numbers right-aligned, consistent decimal places per column
//!   Rounded borders throughout — softer, more professional
//!   Unicode status indicators: ● active  ○ idle  ✗ error
//!
//! Layout:
//!   Header bar (4 rows): BTC prices, connection status, UTC clock, key metrics
//!   Tab bar   (3 rows): 1-6 tab selector
//!   Main area (min):    tab-specific content
//!   Footer    (1 row):  keybindings
//!
//! Tabs:
//!   1 Overview   — 3-col: bankroll stats | BTC dual-feed chart | pipeline+signal
//!   2 Positions  — premium P&L table with time-left countdown
//!   3 Signals    — signal feed log + pipeline funnel
//!   4 Risk       — drawdown gauge + exposure + Kelly bars + regime
//!   5 Analytics  — win rates, profit factor, category breakdown, equity curve
//!   6 AI Advisor — Groq fast-intel | Claude deep-verify split panel

use crate::markets::state::MarketState;
use crate::risk::drawdown::DrawdownTracker;
use crate::risk::regime::{Regime, RegimeState};
use crate::trader::executor::OpenPosition;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Axis, BarChart, Block, BorderType, Borders, Chart, Dataset, Gauge, GraphType,
        Paragraph, Row, Table, Tabs, Wrap,
    },
    Frame, Terminal,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time;
use tui_big_text::{BigText, PixelSize};

// ── Color palette ─────────────────────────────────────────────────────────────

const C_BG: Color = Color::Rgb(13, 13, 13);
const C_PANEL: Color = Color::Rgb(20, 20, 30);
const C_BORDER: Color = Color::Rgb(45, 45, 65);
const C_DIM: Color = Color::Rgb(100, 100, 120);
const C_TEXT: Color = Color::Rgb(210, 210, 220);
const C_BRIGHT: Color = Color::Rgb(245, 245, 255);

const C_GREEN: Color = Color::Rgb(0, 217, 126);
const C_RED: Color = Color::Rgb(255, 68, 68);
const C_AMBER: Color = Color::Rgb(255, 160, 40);
const C_BLUE: Color = Color::Rgb(0, 191, 255);
const C_PURPLE: Color = Color::Rgb(155, 89, 182);
const C_CYAN: Color = Color::Rgb(74, 246, 195);
const C_SLATE: Color = Color::Rgb(123, 104, 238);

// ── Style helpers ─────────────────────────────────────────────────────────────

fn s_dim() -> Style {
    Style::default().fg(C_DIM)
}
fn s_text() -> Style {
    Style::default().fg(C_TEXT)
}
fn s_bright() -> Style {
    Style::default().fg(C_BRIGHT).add_modifier(Modifier::BOLD)
}
fn s_amber() -> Style {
    Style::default().fg(C_AMBER).add_modifier(Modifier::BOLD)
}

/// Rounded panel block. Title dimmed, border subtle.
fn panel(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_BORDER))
        .style(Style::default().bg(C_BG))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(C_DIM).add_modifier(Modifier::BOLD),
        ))
}

/// Panel with amber accent border — used when something is actively alerting.
fn panel_alert(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_AMBER))
        .style(Style::default().bg(C_BG))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(C_AMBER).add_modifier(Modifier::BOLD),
        ))
}

/// P&L colour: green if positive, red if negative, dim if zero.
fn pnl_color(v: Decimal) -> Color {
    if v > Decimal::ZERO {
        C_GREEN
    } else if v < Decimal::ZERO {
        C_RED
    } else {
        C_DIM
    }
}

/// Format P&L with +/- sign and dollar prefix.
fn pnl_str(v: Decimal) -> String {
    if v >= Decimal::ZERO {
        format!("+${:.2}", v)
    } else {
        format!("-${:.2}", v.abs())
    }
}

/// Connection indicator: ● green if connected, ✗ red if not.
fn conn_dot(connected: bool) -> Span<'static> {
    if connected {
        Span::styled("●", Style::default().fg(C_GREEN))
    } else {
        Span::styled("✗", Style::default().fg(C_RED))
    }
}

/// Regime badge: label + color.
fn regime_style(r: &Regime) -> (Color, &'static str) {
    match r {
        Regime::Quiet => (C_GREEN, "QUIET"),
        Regime::Trending => (C_CYAN, "TRENDING"),
        Regime::Volatile => (C_AMBER, "VOLATILE"),
        Regime::Breaking => (C_RED, "BREAKING"),
    }
}

// ── Log types ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub level: LogLevel,
    pub ts: String,
    pub msg: String,
}

impl LogEntry {
    pub fn info(msg: impl Into<String>) -> Self {
        Self {
            level: LogLevel::Info,
            ts: chrono::Utc::now().format("%H:%M:%S").to_string(),
            msg: msg.into(),
        }
    }
    pub fn warn(msg: impl Into<String>) -> Self {
        Self {
            level: LogLevel::Warn,
            ts: chrono::Utc::now().format("%H:%M:%S").to_string(),
            msg: msg.into(),
        }
    }
    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            level: LogLevel::Error,
            ts: chrono::Utc::now().format("%H:%M:%S").to_string(),
            msg: msg.into(),
        }
    }
}

// ── Signal log entry ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SignalLogEntry {
    pub time: String,
    pub market_slug: String,
    pub groq_score: Option<u8>,
    pub claude_score: Option<u8>,
    pub consensus_score: Option<u8>,
    pub kelly_size: Option<Decimal>,
    pub action: String, // "TRADED" | "SKIPPED" | "REJECTED"
}

// ── Dashboard state ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DashboardState {
    // ── Core P&L
    pub total_pnl: Decimal,
    pub today_pnl: Decimal,
    pub wins: u32,
    pub total_trades: u32,
    pub bankroll: Decimal,

    // ── Live positions
    pub positions: Vec<OpenPosition>,

    // ── BTC dual-feed prices (updated by feed tasks)
    pub binance_price: f64,
    pub chainlink_price: f64,
    /// Rolling 5-min BTC price history for Binance (timestamp_secs, price).
    pub binance_history: VecDeque<(f64, f64)>,
    /// Rolling 5-min BTC price history for Chainlink (timestamp_secs, price).
    pub chainlink_history: VecDeque<(f64, f64)>,

    // ── Signal log
    pub signal_log: VecDeque<SignalLogEntry>,

    // ── Equity history (bankroll snapshots)
    pub equity_history: VecDeque<(f64, f64)>,

    // ── Kelly bet sizes (last 20)
    pub kelly_history: VecDeque<Decimal>,

    // ── AI text panels
    pub groq_commentary: String,
    pub claude_reasoning: String,

    // ── Category stats
    pub flash_wins: u32,
    pub flash_total: u32,
    pub standard_wins: u32,
    pub standard_total: u32,
    pub gross_wins: Decimal,
    pub gross_losses: Decimal,

    // ── Pipeline funnel
    pub pipeline_scanned: u32,
    pub pipeline_signals: u32,
    pub pipeline_risk_passed: u32,
    pub pipeline_placed: u32,

    // ── Connection status (set by feed tasks on connect/disconnect)
    pub conn_binance: bool,
    pub conn_chainlink: bool,
    pub conn_clob: bool,
    pub conn_groq: bool,
    pub conn_claude: bool,

    // ── Status log
    pub log_entries: VecDeque<LogEntry>,
}

impl DashboardState {
    pub fn new(initial_bankroll: Decimal) -> Self {
        Self {
            total_pnl: Decimal::ZERO,
            today_pnl: Decimal::ZERO,
            wins: 0,
            total_trades: 0,
            bankroll: initial_bankroll,
            positions: vec![],
            binance_price: 0.0,
            chainlink_price: 0.0,
            binance_history: VecDeque::with_capacity(300),
            chainlink_history: VecDeque::with_capacity(300),
            signal_log: VecDeque::with_capacity(200),
            equity_history: VecDeque::with_capacity(1000),
            kelly_history: VecDeque::with_capacity(20),
            groq_commentary: "Waiting for first Groq update…".into(),
            claude_reasoning: "No Claude reasoning yet.".into(),
            flash_wins: 0,
            flash_total: 0,
            standard_wins: 0,
            standard_total: 0,
            gross_wins: Decimal::ZERO,
            gross_losses: Decimal::ZERO,
            pipeline_scanned: 0,
            pipeline_signals: 0,
            pipeline_risk_passed: 0,
            pipeline_placed: 0,
            conn_binance: false,
            conn_chainlink: false,
            conn_clob: false,
            conn_groq: false,
            conn_claude: false,
            log_entries: VecDeque::with_capacity(100),
        }
    }

    pub fn win_rate(&self) -> f64 {
        if self.total_trades == 0 {
            return 0.0;
        }
        self.wins as f64 / self.total_trades as f64 * 100.0
    }

    pub fn profit_factor(&self) -> Decimal {
        if self.gross_losses == Decimal::ZERO {
            return self.gross_wins;
        }
        self.gross_wins / self.gross_losses
    }

    pub fn log_signal(&mut self, entry: SignalLogEntry) {
        if self.signal_log.len() >= 200 {
            self.signal_log.pop_front();
        }
        self.signal_log.push_back(entry);
    }

    pub fn record_kelly(&mut self, size: Decimal) {
        if self.kelly_history.len() >= 20 {
            self.kelly_history.pop_front();
        }
        self.kelly_history.push_back(size);
    }

    pub fn record_equity(&mut self, bankroll: Decimal) {
        let ts = chrono::Utc::now().timestamp() as f64;
        let val = bankroll.to_string().parse::<f64>().unwrap_or(0.0);
        if self.equity_history.len() >= 1000 {
            self.equity_history.pop_front();
        }
        self.equity_history.push_back((ts, val));
    }

    pub fn push_btc(&mut self, binance: f64, chainlink: f64) {
        let ts = chrono::Utc::now().timestamp() as f64;
        self.binance_price = binance;
        self.chainlink_price = chainlink;
        if self.binance_history.len() >= 300 {
            self.binance_history.pop_front();
        }
        if self.chainlink_history.len() >= 300 {
            self.chainlink_history.pop_front();
        }
        self.binance_history.push_back((ts, binance));
        self.chainlink_history.push_back((ts, chainlink));
    }

    pub fn push_log(&mut self, entry: LogEntry) {
        if self.log_entries.len() >= 100 {
            self.log_entries.pop_front();
        }
        self.log_entries.push_back(entry);
    }
}

/// Thread-safe handle shared across all tasks.
pub type SharedDashState = Arc<Mutex<DashboardState>>;

// ── TUI runner ────────────────────────────────────────────────────────────────

/// Run the dashboard TUI.
///
/// Draws at 10 Hz. Keyboard input processed each frame.
/// Exits on `q` or Ctrl-C.
pub async fn run(
    dash_state: SharedDashState,
    market_state: MarketState,
    drawdown: DrawdownTracker,
    regime: RegimeState,
) {
    enable_raw_mode().expect("enable_raw_mode failed");
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).expect("EnterAlternateScreen failed");
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).expect("Terminal::new failed");

    let mut active_tab: usize = 0;
    let tab_titles = [
        "1 Overview",
        "2 Positions",
        "3 Signals",
        "4 Risk",
        "5 Analytics",
        "6 AI Advisor",
    ];
    let mut tick = time::interval(Duration::from_millis(100));

    loop {
        tick.tick().await;

        let state = dash_state.lock().unwrap().clone();
        let (peak, current_bk, dd_pct, halted) = drawdown.snapshot();
        let regime_now = regime.current();
        let utc_now = chrono::Utc::now().format("%H:%M:%S UTC").to_string();

        terminal
            .draw(|f| {
                draw_frame(
                    f,
                    &state,
                    &market_state,
                    active_tab,
                    &tab_titles,
                    peak,
                    current_bk,
                    dd_pct,
                    halted,
                    &regime_now,
                    &utc_now,
                )
            })
            .ok();

        if event::poll(Duration::from_millis(0)).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read() {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => break,
                    (KeyCode::Char('1'), _) => active_tab = 0,
                    (KeyCode::Char('2'), _) => active_tab = 1,
                    (KeyCode::Char('3'), _) => active_tab = 2,
                    (KeyCode::Char('4'), _) => active_tab = 3,
                    (KeyCode::Char('5'), _) => active_tab = 4,
                    (KeyCode::Char('6'), _) => active_tab = 5,
                    (KeyCode::Left, _) => active_tab = active_tab.saturating_sub(1),
                    (KeyCode::Right, _) => active_tab = (active_tab + 1).min(5),
                    _ => {}
                }
            }
        }
    }

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
}

// ── Frame renderer ────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_frame(
    f: &mut Frame,
    state: &DashboardState,
    market_state: &MarketState,
    active_tab: usize,
    tab_titles: &[&str; 6],
    peak: Decimal,
    current_bk: Decimal,
    dd_pct: Decimal,
    halted: bool,
    regime: &Regime,
    utc_now: &str,
) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // header bar: BTC + connections + metrics
            Constraint::Length(3), // tab selector
            Constraint::Min(0),    // main content
            Constraint::Length(1), // footer keybindings
        ])
        .split(area);

    draw_header(f, chunks[0], state, regime, utc_now);
    draw_tabs(f, chunks[1], tab_titles, active_tab);

    match active_tab {
        0 => draw_overview(f, chunks[2], state, market_state, regime),
        1 => draw_positions(f, chunks[2], state, market_state),
        2 => draw_signals(f, chunks[2], state),
        3 => draw_risk(f, chunks[2], state, peak, current_bk, dd_pct, halted, regime),
        4 => draw_analytics(f, chunks[2], state),
        5 => draw_ai_advisor(f, chunks[2], state, regime),
        _ => {}
    }

    draw_footer(f, chunks[3]);
}

// ── Header bar ────────────────────────────────────────────────────────────────

fn draw_header(f: &mut Frame, area: Rect, state: &DashboardState, regime: &Regime, utc_now: &str) {
    let (regime_color, regime_label) = regime_style(regime);

    // Line 1: app name  BTC prices  connections  UTC time
    let bn_price = if state.binance_price > 0.0 {
        format!("${:.0}", state.binance_price)
    } else {
        "---".into()
    };
    let cl_price = if state.chainlink_price > 0.0 {
        format!("${:.0}", state.chainlink_price)
    } else {
        "---".into()
    };
    let div_pct = if state.binance_price > 0.0 && state.chainlink_price > 0.0 {
        let d = ((state.binance_price - state.chainlink_price) / state.chainlink_price).abs()
            * 100.0;
        format!("{:.3}%", d)
    } else {
        "---".into()
    };
    let div_color = if state.binance_price > 0.0 && state.chainlink_price > 0.0 {
        let d = ((state.binance_price - state.chainlink_price) / state.chainlink_price).abs();
        if d >= 0.003 {
            C_AMBER
        } else {
            C_DIM
        }
    } else {
        C_DIM
    };

    // Connections line
    let conn_items: Vec<(&str, bool)> = vec![
        ("BN", state.conn_binance),
        ("CL", state.conn_chainlink),
        ("WS", state.conn_clob),
        ("GQ", state.conn_groq),
        ("AI", state.conn_claude),
    ];

    // Line 2: bankroll  day P&L  exposure  win rate
    let open_usd: Decimal = state.positions.iter().map(|p| p.size_usd).sum();
    let exp_pct = if open_usd > Decimal::ZERO {
        (open_usd / dec!(40) * dec!(100)).round()
    } else {
        Decimal::ZERO
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_BORDER))
        .style(Style::default().bg(C_BG));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    // Line 1 spans
    let mut line1: Vec<Span> = vec![
        Span::styled("  MANTIS ", Style::default().fg(C_BRIGHT).add_modifier(Modifier::BOLD)),
        Span::styled("│ ", s_dim()),
        Span::styled("BTC  ", s_dim()),
        Span::styled(format!("BN:{bn_price}"), Style::default().fg(C_BLUE)),
        Span::styled("  /  ", s_dim()),
        Span::styled(format!("CL:{cl_price}"), Style::default().fg(C_PURPLE)),
        Span::styled("  div:", s_dim()),
        Span::styled(div_pct, Style::default().fg(div_color)),
        Span::styled("  │  ", s_dim()),
    ];
    for (label, connected) in &conn_items {
        line1.push(Span::styled(label.to_string(), s_dim()));
        line1.push(Span::styled(":", s_dim()));
        line1.push(conn_dot(*connected));
        line1.push(Span::styled("  ", s_dim()));
    }
    line1.push(Span::styled("│  ", s_dim()));
    line1.push(Span::styled(
        format!("Regime: {regime_label}"),
        Style::default().fg(regime_color).add_modifier(Modifier::BOLD),
    ));
    line1.push(Span::styled("  │  ", s_dim()));
    line1.push(Span::styled(utc_now, Style::default().fg(C_DIM)));

    // Line 2 spans
    let day_color = pnl_color(state.today_pnl);
    let total_color = pnl_color(state.total_pnl);
    let exp_color = if exp_pct > dec!(75) {
        C_AMBER
    } else if exp_pct > dec!(50) {
        Color::Rgb(215, 153, 33)
    } else {
        C_GREEN
    };
    let line2 = vec![
        Span::styled("  Bankroll: ", s_dim()),
        Span::styled(
            format!("${:.2}", state.bankroll),
            Style::default().fg(C_BRIGHT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  │  Day P&L: ", s_dim()),
        Span::styled(pnl_str(state.today_pnl), Style::default().fg(day_color).add_modifier(Modifier::BOLD)),
        Span::styled("  │  Total P&L: ", s_dim()),
        Span::styled(pnl_str(state.total_pnl), Style::default().fg(total_color)),
        Span::styled("  │  Exposure: ", s_dim()),
        Span::styled(
            format!("${:.0}/$40 ({exp_pct:.0}%)", open_usd),
            Style::default().fg(exp_color),
        ),
        Span::styled("  │  Win rate: ", s_dim()),
        Span::styled(
            format!("{:.1}%", state.win_rate()),
            Style::default().fg(C_CYAN),
        ),
        Span::styled("  │  Trades: ", s_dim()),
        Span::styled(
            format!("{}", state.total_trades),
            Style::default().fg(C_TEXT),
        ),
        Span::styled("  │  Positions: ", s_dim()),
        Span::styled(
            format!("{}", state.positions.len()),
            Style::default().fg(C_TEXT),
        ),
    ];

    f.render_widget(Paragraph::new(Line::from(line1)), lines_area[0]);
    f.render_widget(Paragraph::new(Line::from(line2)), lines_area[1]);
}

// ── Tab bar ───────────────────────────────────────────────────────────────────

fn draw_tabs(f: &mut Frame, area: Rect, titles: &[&str; 6], active: usize) {
    let lines: Vec<Line> = titles
        .iter()
        .map(|t| Line::from(Span::raw(*t)))
        .collect();
    let tabs = Tabs::new(lines)
        .select(active)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(C_BORDER))
                .style(Style::default().bg(C_BG)),
        )
        .style(s_dim())
        .highlight_style(
            Style::default()
                .fg(C_SLATE)
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::UNDERLINED),
        )
        .divider(Span::styled("  │  ", s_dim()));
    f.render_widget(tabs, area);
}

// ── Footer ────────────────────────────────────────────────────────────────────

fn draw_footer(f: &mut Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled("  [q]", s_amber()),
        Span::styled("uit  ", s_dim()),
        Span::styled("[1-6]", s_amber()),
        Span::styled("tabs  ", s_dim()),
        Span::styled("[←→]", s_amber()),
        Span::styled("navigate", s_dim()),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(C_BG)),
        area,
    );
}

// ── TAB 1: Overview ───────────────────────────────────────────────────────────

fn draw_overview(
    f: &mut Frame,
    area: Rect,
    state: &DashboardState,
    market_state: &MarketState,
    regime: &Regime,
) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(26),
            Constraint::Percentage(48),
            Constraint::Percentage(26),
        ])
        .split(area);

    draw_overview_left(f, cols[0], state);
    draw_overview_center(f, cols[1], state, regime);
    draw_overview_right(f, cols[2], state, market_state);
}

fn draw_overview_left(f: &mut Frame, area: Rect, state: &DashboardState) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // big bankroll number
            Constraint::Length(5), // P&L stats
            Constraint::Min(0),    // connection status
        ])
        .split(area);

    // Bankroll big text
    let bk_color = if state.bankroll >= dec!(100) {
        C_GREEN
    } else {
        C_AMBER
    };
    let bk_text = BigText::builder()
        .pixel_size(PixelSize::HalfHeight)
        .lines(vec![Line::from(Span::styled(
            format!("${:.2}", state.bankroll),
            Style::default().fg(bk_color).add_modifier(Modifier::BOLD),
        ))])
        .build();
    let bk_block = panel("BANKROLL");
    let inner = bk_block.inner(rows[0]);
    f.render_widget(bk_block, rows[0]);
    f.render_widget(bk_text, inner);

    // P&L stats
    let day_c = pnl_color(state.today_pnl);
    let total_c = pnl_color(state.total_pnl);
    let lines = vec![
        Line::from(vec![
            Span::styled("  Day P&L  ", s_dim()),
            Span::styled(pnl_str(state.today_pnl), Style::default().fg(day_c).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled("  Total    ", s_dim()),
            Span::styled(pnl_str(state.total_pnl), Style::default().fg(total_c)),
        ]),
        Line::from(vec![
            Span::styled("  Wins     ", s_dim()),
            Span::styled(format!("{}/{}", state.wins, state.total_trades), Style::default().fg(C_CYAN)),
        ]),
    ];
    f.render_widget(
        Paragraph::new(lines).block(panel("P&L")).style(s_text()),
        rows[1],
    );

    // Connection status
    let conn_lines = vec![
        Line::from(vec![
            Span::styled("  Binance  ", s_dim()),
            conn_dot(state.conn_binance),
            Span::styled(if state.conn_binance { " live" } else { " offline" }, s_dim()),
        ]),
        Line::from(vec![
            Span::styled("  Chainlink", s_dim()),
            conn_dot(state.conn_chainlink),
            Span::styled(if state.conn_chainlink { " live" } else { " offline" }, s_dim()),
        ]),
        Line::from(vec![
            Span::styled("  CLOB WS  ", s_dim()),
            conn_dot(state.conn_clob),
            Span::styled(if state.conn_clob { " live" } else { " offline" }, s_dim()),
        ]),
        Line::from(vec![
            Span::styled("  Groq     ", s_dim()),
            conn_dot(state.conn_groq),
            Span::styled(if state.conn_groq { " active" } else { " unused" }, s_dim()),
        ]),
        Line::from(vec![
            Span::styled("  Claude   ", s_dim()),
            conn_dot(state.conn_claude),
            Span::styled(if state.conn_claude { " active" } else { " unused" }, s_dim()),
        ]),
    ];
    f.render_widget(
        Paragraph::new(conn_lines).block(panel("CONNECTIONS")).style(s_text()),
        rows[2],
    );
}

fn draw_overview_center(f: &mut Frame, area: Rect, state: &DashboardState, regime: &Regime) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(area);

    // BTC dual-feed chart
    let has_data = state.binance_history.len() >= 2 && state.chainlink_history.len() >= 2;
    if has_data {
        let bn_data: Vec<(f64, f64)> = state.binance_history.iter().cloned().collect();
        let cl_data: Vec<(f64, f64)> = state.chainlink_history.iter().cloned().collect();

        let all_prices: Vec<f64> = bn_data
            .iter()
            .chain(cl_data.iter())
            .map(|(_, p)| *p)
            .collect();
        let min_p = all_prices.iter().cloned().fold(f64::INFINITY, f64::min) * 0.9995;
        let max_p = all_prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max) * 1.0005;

        let x_min = bn_data.first().map(|(t, _)| *t).unwrap_or(0.0);
        let x_max = bn_data.last().map(|(t, _)| *t).unwrap_or(1.0);

        // Detect divergence — amber if >= 0.3%
        let bn_last = state.binance_price;
        let cl_last = state.chainlink_price;
        let div = if cl_last > 0.0 {
            ((bn_last - cl_last) / cl_last).abs()
        } else {
            0.0
        };
        let bn_color = if div >= 0.003 { C_AMBER } else { C_BLUE };
        let cl_color = if div >= 0.003 { C_AMBER } else { C_PURPLE };

        let datasets = vec![
            Dataset::default()
                .name("Binance")
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(bn_color))
                .data(&bn_data),
            Dataset::default()
                .name("Chainlink")
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(cl_color))
                .data(&cl_data),
        ];

        let chart = Chart::new(datasets)
            .block(panel("BTC / USD  ─  Binance + Chainlink  (5 min)"))
            .x_axis(
                Axis::default()
                    .style(s_dim())
                    .bounds([x_min, x_max]),
            )
            .y_axis(
                Axis::default()
                    .style(s_dim())
                    .bounds([min_p, max_p])
                    .labels(vec![
                        Span::styled(format!("{:.0}", min_p), s_dim()),
                        Span::styled(format!("{:.0}", max_p), s_dim()),
                    ]),
            );
        f.render_widget(chart, rows[0]);
    } else {
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                "  Waiting for BTC feed data…",
                s_dim(),
            )]))
            .block(panel("BTC / USD  ─  Binance + Chainlink  (5 min)"))
            .alignment(Alignment::Left),
            rows[0],
        );
    }

    // Divergence + regime strip
    let (regime_color, regime_label) = regime_style(regime);
    let div_display = if state.binance_price > 0.0 && state.chainlink_price > 0.0 {
        let d = ((state.binance_price - state.chainlink_price) / state.chainlink_price).abs()
            * 100.0;
        let signal = if d >= 0.3 { "  ⚡ FLASH SIGNAL ACTIVE" } else { "" };
        (
            format!("  Divergence: {:.3}%{signal}", d),
            if d >= 0.3 { C_AMBER } else { C_DIM },
        )
    } else {
        ("  Divergence: waiting…".into(), C_DIM)
    };
    let div_line = Line::from(vec![
        Span::styled(div_display.0, Style::default().fg(div_display.1)),
        Span::styled("     Regime: ", s_dim()),
        Span::styled(
            regime_label,
            Style::default().fg(regime_color).add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(
        Paragraph::new(div_line).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(C_BORDER))
                .style(Style::default().bg(C_BG)),
        ),
        rows[1],
    );
}

fn draw_overview_right(
    f: &mut Frame,
    area: Rect,
    state: &DashboardState,
    _market_state: &MarketState,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Min(0)])
        .split(area);

    // Pipeline funnel
    let total = state.pipeline_scanned.max(1) as f64;
    let funnel_lines = vec![
        funnel_row("Scanned", state.pipeline_scanned, state.pipeline_scanned, total),
        funnel_row("Signals", state.pipeline_signals, state.pipeline_scanned, total),
        funnel_row("Risk OK", state.pipeline_risk_passed, state.pipeline_scanned, total),
        funnel_row("Placed ", state.pipeline_placed, state.pipeline_scanned, total),
    ];
    f.render_widget(
        Paragraph::new(funnel_lines).block(panel("PIPELINE FUNNEL")).style(s_text()),
        rows[0],
    );

    // Active signal (most recent TRADED entry)
    let last_trade = state
        .signal_log
        .iter()
        .rev()
        .find(|e| e.action == "TRADED");

    if let Some(sig) = last_trade {
        let groq = sig.groq_score.map(|s| format!("{s}")).unwrap_or_else(|| "--".into());
        let claude = sig.claude_score.map(|s| format!("{s}")).unwrap_or_else(|| "--".into());
        let kelly = sig
            .kelly_size
            .map(|s| format!("${:.2}", s))
            .unwrap_or_else(|| "--".into());
        let lines = vec![
            Line::from(Span::styled(
                format!("  {:.20}", sig.market_slug),
                Style::default().fg(C_AMBER).add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled("  Groq:   ", s_dim()),
                Span::styled(groq, Style::default().fg(C_CYAN)),
            ]),
            Line::from(vec![
                Span::styled("  Claude: ", s_dim()),
                Span::styled(claude, Style::default().fg(C_PURPLE)),
            ]),
            Line::from(vec![
                Span::styled("  Kelly:  ", s_dim()),
                Span::styled(kelly, Style::default().fg(C_GREEN).add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled("  At:     ", s_dim()),
                Span::styled(sig.time.clone(), s_dim()),
            ]),
        ];
        f.render_widget(
            Paragraph::new(lines).block(panel_alert("LAST TRADE")),
            rows[1],
        );
    } else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  Waiting for first trade…",
                s_dim(),
            )))
            .block(panel("LAST TRADE")),
            rows[1],
        );
    }
}

/// Build a single pipeline funnel row with a proportional bar.
fn funnel_row<'a>(label: &'a str, count: u32, total: u32, _max: f64) -> Line<'a> {
    let pct = if total == 0 {
        0.0
    } else {
        count as f64 / total as f64
    };
    let bar_width: usize = 12;
    let filled = (pct * bar_width as f64).round() as usize;
    let bar: String = "█".repeat(filled) + &"░".repeat(bar_width - filled);
    Line::from(vec![
        Span::styled(format!("  {label} "), s_dim()),
        Span::styled(bar, Style::default().fg(C_SLATE)),
        Span::styled(format!(" {count:>4}"), Style::default().fg(C_BRIGHT)),
    ])
}

// ── TAB 2: Positions ──────────────────────────────────────────────────────────

fn draw_positions(f: &mut Frame, area: Rect, state: &DashboardState, market_state: &MarketState) {
    if state.positions.is_empty() {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  No open positions — agent is in cash.",
                    s_dim(),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Cash is a valid position. MANTIS waits for high-certainty edge.",
                    s_dim(),
                )),
            ])
            .block(panel("OPEN POSITIONS  (0)"))
            .alignment(Alignment::Left),
            area,
        );
        return;
    }

    let header = Row::new(vec![
        " #", "Market", "Dir", "Entry", "Current", "P&L%", "Size USD", "Opened", "Time Left",
    ])
    .style(Style::default().fg(C_AMBER).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = state
        .positions
        .iter()
        .enumerate()
        .map(|(i, pos)| {
            let current_price = market_state
                .markets
                .get(&pos.condition_id)
                .map(|s| {
                    if pos.direction == "YES" {
                        s.yes_price
                    } else {
                        dec!(1) - s.yes_price
                    }
                })
                .unwrap_or(pos.entry_price);

            let pnl_pct = if pos.entry_price > Decimal::ZERO {
                (current_price - pos.entry_price) / pos.entry_price * dec!(100)
            } else {
                Decimal::ZERO
            };

            let secs = market_state
                .markets
                .get(&pos.condition_id)
                .map(|s| s.seconds_to_resolution())
                .unwrap_or(0);
            let time_str = format!("{:>3}m{:02}s", secs / 60, secs % 60);
            let slug = &pos.condition_id[..8.min(pos.condition_id.len())];
            let opened = pos.opened_at.format("%H:%M:%S").to_string();

            let row_color = pnl_color(pnl_pct);
            Row::new(vec![
                format!(" {}", i + 1),
                slug.to_string(),
                pos.direction.clone(),
                format!("{:.3}", pos.entry_price),
                format!("{:.3}", current_price),
                format!("{:>+6.1}%", pnl_pct),
                format!("${:.2}", pos.size_usd),
                opened,
                time_str,
            ])
            .style(Style::default().fg(row_color))
        })
        .collect();

    let widths = [
        Constraint::Length(3),
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(9),
        Constraint::Length(10),
        Constraint::Length(10),
    ];

    let open_usd: Decimal = state.positions.iter().map(|p| p.size_usd).sum();
    let open_pnl: Decimal = state
        .positions
        .iter()
        .map(|pos| {
            let current = market_state
                .markets
                .get(&pos.condition_id)
                .map(|s| {
                    if pos.direction == "YES" {
                        s.yes_price
                    } else {
                        dec!(1) - s.yes_price
                    }
                })
                .unwrap_or(pos.entry_price);
            if pos.entry_price > Decimal::ZERO {
                (current - pos.entry_price) / pos.entry_price * pos.size_usd
            } else {
                Decimal::ZERO
            }
        })
        .sum();

    let title = format!(
        "OPEN POSITIONS  ({})   Exposure: ${:.2}   Unrealised: {}",
        state.positions.len(),
        open_usd,
        pnl_str(open_pnl),
    );

    let table = Table::new(rows, widths)
        .header(header)
        .block(panel(&title))
        .column_spacing(1)
        .style(s_text());
    f.render_widget(table, area);
}

// ── TAB 3: Signals ────────────────────────────────────────────────────────────

fn draw_signals(f: &mut Frame, area: Rect, state: &DashboardState) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(area);

    // Signal feed (top)
    let header = Row::new(vec!["Time", "Market", "Groq", "Claude", "Consensus", "Kelly", "Action"])
        .style(Style::default().fg(C_AMBER).add_modifier(Modifier::BOLD));

    let sig_rows: Vec<Row> = state
        .signal_log
        .iter()
        .rev()
        .take(50)
        .map(|e| {
            let color = match e.action.as_str() {
                "TRADED" => C_GREEN,
                "REJECTED" => C_RED,
                _ => C_DIM,
            };
            let groq = e.groq_score.map(|s| format!("{s:>3}")).unwrap_or_else(|| " --".into());
            let claude = e.claude_score.map(|s| format!("{s:>3}")).unwrap_or_else(|| " --".into());
            let cons = e.consensus_score.map(|s| format!("{s:>3}")).unwrap_or_else(|| " --".into());
            let kelly = e.kelly_size.map(|s| format!("${s:.2}")).unwrap_or_else(|| "   --".into());
            Row::new(vec![
                e.time.clone(),
                format!("{:.18}", e.market_slug),
                groq,
                claude,
                cons,
                kelly,
                e.action.clone(),
            ])
            .style(Style::default().fg(color))
        })
        .collect();

    let widths = [
        Constraint::Length(9),
        Constraint::Length(20),
        Constraint::Length(5),
        Constraint::Length(7),
        Constraint::Length(10),
        Constraint::Length(7),
        Constraint::Length(9),
    ];

    let title = format!("SIGNAL FEED  ({} total)", state.signal_log.len());
    f.render_widget(
        Table::new(sig_rows, widths).header(header).block(panel(&title)).style(s_text()),
        rows[0],
    );

    // Pipeline + category stats (bottom, split)
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    let total = state.pipeline_scanned.max(1) as f64;
    let funnel_lines = vec![
        funnel_row("Scanned", state.pipeline_scanned, state.pipeline_scanned, total),
        funnel_row("Signals", state.pipeline_signals, state.pipeline_scanned, total),
        funnel_row("Risk OK", state.pipeline_risk_passed, state.pipeline_scanned, total),
        funnel_row("Placed ", state.pipeline_placed, state.pipeline_scanned, total),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Rejection rate: ", s_dim()),
            Span::styled(
                format!(
                    "{:.0}%",
                    if state.pipeline_signals > 0 {
                        (1.0 - state.pipeline_placed as f64 / state.pipeline_signals as f64)
                            * 100.0
                    } else {
                        0.0
                    }
                ),
                Style::default().fg(C_CYAN),
            ),
        ]),
    ];
    f.render_widget(
        Paragraph::new(funnel_lines).block(panel("PIPELINE FUNNEL")).style(s_text()),
        bottom[0],
    );

    let flash_wr = if state.flash_total > 0 {
        format!("{:.1}%", state.flash_wins as f64 / state.flash_total as f64 * 100.0)
    } else {
        "N/A".into()
    };
    let std_wr = if state.standard_total > 0 {
        format!("{:.1}%", state.standard_wins as f64 / state.standard_total as f64 * 100.0)
    } else {
        "N/A".into()
    };
    let cat_lines = vec![
        Line::from(vec![
            Span::styled("  Flash   win rate: ", s_dim()),
            Span::styled(&flash_wr, Style::default().fg(C_BLUE).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  ({}/{})", state.flash_wins, state.flash_total), s_dim()),
        ]),
        Line::from(vec![
            Span::styled("  Std     win rate: ", s_dim()),
            Span::styled(&std_wr, Style::default().fg(C_PURPLE).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  ({}/{})", state.standard_wins, state.standard_total), s_dim()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Profit factor:    ", s_dim()),
            Span::styled(
                format!("{:.2}x", state.profit_factor()),
                Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Gross wins:       ", s_dim()),
            Span::styled(format!("${:.2}", state.gross_wins), Style::default().fg(C_GREEN)),
        ]),
        Line::from(vec![
            Span::styled("  Gross losses:     ", s_dim()),
            Span::styled(format!("${:.2}", state.gross_losses), Style::default().fg(C_RED)),
        ]),
    ];
    f.render_widget(
        Paragraph::new(cat_lines).block(panel("CATEGORY STATS")).style(s_text()),
        bottom[1],
    );
}

// ── TAB 4: Risk ───────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_risk(
    f: &mut Frame,
    area: Rect,
    state: &DashboardState,
    peak: Decimal,
    current_bk: Decimal,
    dd_pct: Decimal,
    halted: bool,
    regime: &Regime,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Length(4), Constraint::Min(0)])
        .split(area);

    // ── Drawdown gauge
    let dd_f64: f64 = dd_pct.to_string().parse().unwrap_or(0.0);
    let dd_color = if dd_f64 >= 0.15 {
        C_RED
    } else if dd_f64 >= 0.10 {
        C_AMBER
    } else if dd_f64 >= 0.05 {
        Color::Rgb(215, 153, 33)
    } else {
        C_GREEN
    };
    let halt_tag = if halted { "  ⚠ TRADING HALTED" } else { "" };
    let dd_title = format!(
        "DRAWDOWN  peak=${:.2}  current=${:.2}{halt_tag}",
        peak, current_bk
    );
    let dd_block = if halted {
        panel_alert(&dd_title)
    } else {
        panel(&dd_title)
    };
    let dd_inner = dd_block.inner(rows[0]);
    f.render_widget(dd_block, rows[0]);
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(dd_color).bg(C_PANEL))
            .ratio((dd_f64 / 0.20_f64).min(1.0))
            .label(Span::styled(
                format!("{:.2}% / 20.0% halt", dd_f64 * 100.0),
                Style::default().fg(C_BRIGHT).add_modifier(Modifier::BOLD),
            )),
        dd_inner,
    );

    // ── Exposure + regime
    let open_usd: Decimal = state.positions.iter().map(|p| p.size_usd).sum();
    let exp_f64: f64 = (open_usd / dec!(40)).to_string().parse().unwrap_or(0.0);
    let exp_color = if exp_f64 >= 0.75 { C_RED } else if exp_f64 >= 0.50 { C_AMBER } else { C_GREEN };
    let (regime_color, regime_label) = regime_style(regime);

    let exp_block = panel("EXPOSURE & REGIME");
    let exp_inner = exp_block.inner(rows[1]);
    f.render_widget(exp_block, rows[1]);

    let exp_lines_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(exp_inner);
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(exp_color).bg(C_PANEL))
            .ratio(exp_f64.min(1.0))
            .label(Span::styled(
                format!("${:.2} / $40.00 exposure  ({} positions)", open_usd, state.positions.len()),
                Style::default().fg(C_BRIGHT),
            )),
        exp_lines_area[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Regime: ", s_dim()),
            Span::styled(
                regime_label,
                Style::default().fg(regime_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   Today P&L: ", s_dim()),
            Span::styled(
                pnl_str(state.today_pnl),
                Style::default()
                    .fg(pnl_color(state.today_pnl))
                    .add_modifier(Modifier::BOLD),
            ),
        ])),
        exp_lines_area[1],
    );

    // ── Kelly bet sizes bar chart
    if !state.kelly_history.is_empty() {
        let bar_data: Vec<(&str, u64)> = state
            .kelly_history
            .iter()
            .map(|v| {
                let cents = (v.to_string().parse::<f64>().unwrap_or(0.0) * 100.0) as u64;
                ("", cents)
            })
            .collect();
        f.render_widget(
            BarChart::default()
                .block(panel("KELLY BET SIZES  (last 20, $ × 100)"))
                .data(&bar_data)
                .bar_width(4)
                .bar_gap(1)
                .bar_style(Style::default().fg(C_SLATE))
                .value_style(Style::default().fg(C_BRIGHT))
                .label_style(s_dim()),
            rows[2],
        );
    } else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  No bets placed yet — Kelly sizes will appear here.",
                s_dim(),
            )))
            .block(panel("KELLY BET SIZES  (last 20)")),
            rows[2],
        );
    }
}

// ── TAB 5: Analytics ──────────────────────────────────────────────────────────

fn draw_analytics(f: &mut Frame, area: Rect, state: &DashboardState) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    // Left: stats table
    let flash_wr = if state.flash_total > 0 {
        format!("{:.1}%", state.flash_wins as f64 / state.flash_total as f64 * 100.0)
    } else {
        "N/A".into()
    };
    let std_wr = if state.standard_total > 0 {
        format!("{:.1}%", state.standard_wins as f64 / state.standard_total as f64 * 100.0)
    } else {
        "N/A".into()
    };

    let v_total     = state.total_trades.to_string();
    let v_wr        = format!("{:.1}%", state.win_rate());
    let v_flash_tr  = format!("{}/{}", state.flash_wins, state.flash_total);
    let v_std_tr    = format!("{}/{}", state.standard_wins, state.standard_total);
    let v_pf        = format!("{:.2}x", state.profit_factor());
    let v_gw        = format!("${:.2}", state.gross_wins);
    let v_gl        = format!("${:.2}", state.gross_losses);
    let v_tpnl      = pnl_str(state.total_pnl);
    let v_dpnl      = pnl_str(state.today_pnl);
    let v_bk        = format!("${:.2}", state.bankroll);
    let tc_tpnl     = pnl_color(state.total_pnl);
    let tc_dpnl     = pnl_color(state.today_pnl);

    let rows = vec![
        stat_row("Total trades",    &v_total,    C_BRIGHT),
        stat_row("Overall win rate",&v_wr,        C_CYAN),
        stat_row("Flash win rate",  &flash_wr,    C_BLUE),
        stat_row("Std win rate",    &std_wr,      C_PURPLE),
        stat_row("Flash trades",    &v_flash_tr,  C_DIM),
        stat_row("Std trades",      &v_std_tr,    C_DIM),
        stat_row("",                "",           C_DIM),
        stat_row("Profit factor",   &v_pf,        C_CYAN),
        stat_row("Gross wins",      &v_gw,        C_GREEN),
        stat_row("Gross losses",    &v_gl,        C_RED),
        stat_row("",                "",           C_DIM),
        stat_row("Total P&L",       &v_tpnl,      tc_tpnl),
        stat_row("Today P&L",       &v_dpnl,      tc_dpnl),
        stat_row("Bankroll",        &v_bk,        C_BRIGHT),
    ];

    let widths = [Constraint::Percentage(55), Constraint::Percentage(45)];
    f.render_widget(
        Table::new(rows, widths)
            .block(panel("PERFORMANCE STATS"))
            .style(s_text()),
        cols[0],
    );

    // Right: equity curve
    if state.equity_history.len() >= 2 {
        let data: Vec<(f64, f64)> = state.equity_history.iter().cloned().collect();
        let min_v = data.iter().map(|(_, v)| *v).fold(f64::INFINITY, f64::min) * 0.998;
        let max_v = data.iter().map(|(_, v)| *v).fold(f64::NEG_INFINITY, f64::max) * 1.002;
        let x_min = data.first().map(|(t, _)| *t).unwrap_or(0.0);
        let x_max = data.last().map(|(t, _)| *t).unwrap_or(1.0);

        let curve_color = if state.total_pnl >= Decimal::ZERO { C_GREEN } else { C_RED };
        let dataset = Dataset::default()
            .name("Bankroll")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(curve_color))
            .data(&data);

        f.render_widget(
            Chart::new(vec![dataset])
                .block(panel("EQUITY CURVE"))
                .x_axis(Axis::default().style(s_dim()).bounds([x_min, x_max]))
                .y_axis(
                    Axis::default()
                        .style(s_dim())
                        .bounds([min_v, max_v])
                        .labels(vec![
                            Span::styled(format!("${:.2}", min_v), s_dim()),
                            Span::styled(format!("${:.2}", max_v), s_dim()),
                        ]),
                ),
            cols[1],
        );
    } else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  Equity curve populates after first trade.",
                s_dim(),
            )))
            .block(panel("EQUITY CURVE")),
            cols[1],
        );
    }
}

/// Single analytics stat row: label (dim) | value (colored).
fn stat_row<'a>(label: &'a str, value: &'a str, color: Color) -> Row<'a> {
    Row::new(vec![
        Span::styled(format!("  {label}"), s_dim()),
        Span::styled(value.to_string(), Style::default().fg(color).add_modifier(Modifier::BOLD)),
    ])
}

// ── TAB 6: AI Advisor ────────────────────────────────────────────────────────

fn draw_ai_advisor(f: &mut Frame, area: Rect, state: &DashboardState, regime: &Regime) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let (regime_color, regime_label) = regime_style(regime);
    let regime_plain = match regime {
        Regime::Quiet => "Low volume, tight spreads. Baseline Kelly sizing active.",
        Regime::Trending => "Both feeds agree on direction. Size boost active (1.2×).",
        Regime::Volatile => "Large price swings. Sizes reduced (0.5×). Flash threshold raised.",
        Regime::Breaking => "Volume spike — breaking news. Flash orders paused (60s). Re-scoring.",
    };

    // Groq panel
    let groq_lines = vec![
        Line::from(Span::styled("  Groq llama-4-scout  (fast-pass scorer)", s_dim())),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", state.groq_commentary),
            Style::default().fg(C_TEXT),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Regime: ", s_dim()),
            Span::styled(
                regime_label,
                Style::default().fg(regime_color).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            format!("  {regime_plain}"),
            Style::default().fg(C_DIM),
        )),
    ];
    f.render_widget(
        Paragraph::new(groq_lines)
            .block(panel("GROQ  ─  Fast Intel"))
            .wrap(Wrap { trim: true }),
        cols[0],
    );

    // Claude panel
    let claude_lines = vec![
        Line::from(Span::styled("  Claude claude-sonnet  (deep-verify)", s_dim())),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", state.claude_reasoning),
            Style::default().fg(C_TEXT),
        )),
    ];
    f.render_widget(
        Paragraph::new(claude_lines)
            .block(panel("CLAUDE  ─  Deep Verify"))
            .wrap(Wrap { trim: true }),
        cols[1],
    );
}
