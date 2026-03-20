#![allow(dead_code)]
//! 6-tab ratatui dashboard.
//!
//! Keyboard navigation: keys 1-6 switch tabs. 'q' or Ctrl-C exits.
//!
//! TAB 1 — OVERVIEW:    P&L, win rate, Sharpe, Kelly%, regime, equity sparkline
//! TAB 2 — POSITIONS:   live table with price, unrealised P&L, exit strategy, time left
//! TAB 3 — SIGNALS:     live scroll of Groq/Claude scores and actions taken
//! TAB 4 — RISK:        Kelly bar chart, drawdown gauge, exposure meter, regime badge
//! TAB 5 — ANALYTICS:   Brier score, win rate by category, profit factor, best/worst
//! TAB 6 — AI ADVISOR:  Groq commentary + Claude reasoning log + regime plain English

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
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Axis, BarChart, Block, Borders, Chart, Dataset, Gauge, GraphType, List, ListItem,
        Paragraph, Row, Table, Tabs,
    },
    Frame, Terminal,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time;

// ── Shared dashboard state ────────────────────────────────────────────────────

/// All data the dashboard needs, updated by the trader/signal tasks.
#[derive(Debug, Clone)]
pub struct DashboardState {
    /// Realised P&L all-time.
    pub total_pnl: Decimal,
    /// Realised P&L today (UTC).
    pub today_pnl: Decimal,
    /// Win count / total settled trades.
    pub wins: u32,
    pub total_trades: u32,
    /// Current bankroll.
    pub bankroll: Decimal,
    /// Current open positions.
    pub positions: Vec<OpenPosition>,
    /// Signal log: (market_slug, groq_score, claude_score, action, kelly_size).
    pub signal_log: VecDeque<SignalLogEntry>,
    /// Last 7-day bankroll history for sparkline (timestamp_secs, value).
    pub equity_history: VecDeque<(f64, f64)>,
    /// Last 20 Kelly bet sizes for bar chart.
    pub kelly_history: VecDeque<Decimal>,
    /// Groq live commentary (updates every 30s).
    pub groq_commentary: String,
    /// Last Claude reasoning text.
    pub claude_reasoning: String,
    /// Win counts by category.
    pub flash_wins: u32,
    pub flash_total: u32,
    pub standard_wins: u32,
    pub standard_total: u32,
    /// Gross wins / gross losses (profit factor).
    pub gross_wins: Decimal,
    pub gross_losses: Decimal,
}

#[derive(Debug, Clone)]
pub struct SignalLogEntry {
    pub time: String,
    pub market_slug: String,
    pub groq_score: Option<u8>,
    pub claude_score: Option<u8>,
    pub consensus_score: Option<u8>,
    pub kelly_size: Option<Decimal>,
    pub action: String, // "TRADED", "SKIPPED", "REJECTED"
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
            signal_log: VecDeque::with_capacity(200),
            equity_history: VecDeque::with_capacity(1000),
            kelly_history: VecDeque::with_capacity(20),
            groq_commentary: "Waiting for first Groq update...".to_string(),
            claude_reasoning: "No Claude reasoning yet.".to_string(),
            flash_wins: 0,
            flash_total: 0,
            standard_wins: 0,
            standard_total: 0,
            gross_wins: Decimal::ZERO,
            gross_losses: Decimal::ZERO,
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

    /// Add a signal log entry, capped at 200 entries.
    pub fn log_signal(&mut self, entry: SignalLogEntry) {
        if self.signal_log.len() >= 200 {
            self.signal_log.pop_front();
        }
        self.signal_log.push_back(entry);
    }

    /// Record a Kelly bet size for the bar chart.
    pub fn record_kelly(&mut self, size: Decimal) {
        if self.kelly_history.len() >= 20 {
            self.kelly_history.pop_front();
        }
        self.kelly_history.push_back(size);
    }

    /// Record current bankroll for equity sparkline.
    pub fn record_equity(&mut self, bankroll: Decimal) {
        let ts = chrono::Utc::now().timestamp() as f64;
        let val = bankroll.to_string().parse::<f64>().unwrap_or(0.0);
        if self.equity_history.len() >= 1000 {
            self.equity_history.pop_front();
        }
        self.equity_history.push_back((ts, val));
    }
}

/// Thread-safe handle to DashboardState.
pub type SharedDashState = Arc<Mutex<DashboardState>>;

// ── TUI runner ────────────────────────────────────────────────────────────────

/// Run the dashboard TUI forever.
///
/// Draws at 10Hz (100ms refresh). Keyboard input processed each frame.
/// Exits on 'q' or Ctrl-C.
pub async fn run(
    dash_state: SharedDashState,
    market_state: MarketState,
    drawdown: DrawdownTracker,
    regime: RegimeState,
) {
    // Set up terminal.
    enable_raw_mode().expect("enable_raw_mode failed");
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).expect("EnterAlternateScreen failed");
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).expect("Terminal::new failed");

    let mut active_tab: usize = 0;
    let tab_titles = ["1:Overview", "2:Positions", "3:Signals", "4:Risk", "5:Analytics", "6:AI"];
    let mut tick = time::interval(Duration::from_millis(100));

    loop {
        tick.tick().await;

        // Clone state once per frame to release lock quickly.
        let state = dash_state.lock().unwrap().clone();
        let (peak, current_bk, dd_pct, halted) = drawdown.snapshot();
        let regime_current = regime.current();

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
                    &regime_current,
                )
            })
            .ok();

        // Non-blocking key read.
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

    // Restore terminal.
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
) {
    let area = f.area();

    // Top bar: tabs.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    let titles: Vec<Line> = tab_titles
        .iter()
        .map(|t| Line::from(Span::raw(*t)))
        .collect();
    let tabs = Tabs::new(titles)
        .select(active_tab)
        .block(Block::default().borders(Borders::ALL).title(" MANTIS "))
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
    f.render_widget(tabs, chunks[0]);

    match active_tab {
        0 => draw_overview(f, chunks[1], state, regime),
        1 => draw_positions(f, chunks[1], state, market_state),
        2 => draw_signals(f, chunks[1], state),
        3 => draw_risk(f, chunks[1], state, peak, current_bk, dd_pct, halted, regime),
        4 => draw_analytics(f, chunks[1], state),
        5 => draw_ai_advisor(f, chunks[1], state, regime),
        _ => {}
    }
}

// ── TAB 1: Overview ───────────────────────────────────────────────────────────

fn draw_overview(f: &mut Frame, area: Rect, state: &DashboardState, regime: &Regime) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(0)])
        .split(area);

    // Stats row.
    let regime_label = match regime {
        Regime::Quiet => "QUIET",
        Regime::Trending => "TRENDING",
        Regime::Volatile => "VOLATILE",
        Regime::Breaking => "BREAKING",
    };
    let regime_color = match regime {
        Regime::Quiet => Color::Green,
        Regime::Trending => Color::Cyan,
        Regime::Volatile => Color::Yellow,
        Regime::Breaking => Color::Red,
    };

    let stats = format!(
        "Total P&L: {:>+8.2}  |  Today: {:>+8.2}  |  Win Rate: {:>5.1}%  |  Bankroll: {:>8.2}  |  Regime: {}",
        state.total_pnl,
        state.today_pnl,
        state.win_rate(),
        state.bankroll,
        regime_label,
    );

    let para = Paragraph::new(stats)
        .block(Block::default().borders(Borders::ALL).title("Overview"))
        .style(Style::default().fg(regime_color));
    f.render_widget(para, chunks[0]);

    // Equity sparkline.
    if state.equity_history.len() >= 2 {
        let data: Vec<(f64, f64)> = state.equity_history.iter().cloned().collect();
        let min_val = data.iter().map(|(_, v)| *v).fold(f64::INFINITY, f64::min);
        let max_val = data.iter().map(|(_, v)| *v).fold(f64::NEG_INFINITY, f64::max);
        let dataset = Dataset::default()
            .name("Bankroll")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Green))
            .data(&data);
        let chart = Chart::new(vec![dataset])
            .block(Block::default().borders(Borders::ALL).title("Equity Curve (7d)"))
            .x_axis(Axis::default().bounds([data.first().map(|(t, _)| *t).unwrap_or(0.0), data.last().map(|(t, _)| *t).unwrap_or(1.0)]))
            .y_axis(Axis::default().bounds([min_val * 0.99, max_val * 1.01]));
        f.render_widget(chart, chunks[1]);
    } else {
        let para = Paragraph::new("No equity history yet — trades will populate this chart.")
            .block(Block::default().borders(Borders::ALL).title("Equity Curve (7d)"));
        f.render_widget(para, chunks[1]);
    }
}

// ── TAB 2: Live Positions ─────────────────────────────────────────────────────

fn draw_positions(f: &mut Frame, area: Rect, state: &DashboardState, market_state: &MarketState) {
    let header = Row::new(vec!["Market", "Dir", "Entry", "Current", "P&L%", "Size", "Time Left"])
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = state
        .positions
        .iter()
        .map(|pos| {
            let current_price = market_state
                .markets
                .get(&pos.condition_id)
                .map(|s| {
                    if pos.direction == "YES" { s.yes_price } else { dec!(1) - s.yes_price }
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
            let time_str = format!("{}m{}s", secs / 60, secs % 60);

            let color = if pnl_pct > Decimal::ZERO {
                Color::Green
            } else if pnl_pct < Decimal::ZERO {
                Color::Red
            } else {
                Color::White
            };

            Row::new(vec![
                pos.condition_id[..8.min(pos.condition_id.len())].to_string(),
                pos.direction.clone(),
                format!("{:.3}", pos.entry_price),
                format!("{:.3}", current_price),
                format!("{:+.1}%", pnl_pct),
                format!("${:.2}", pos.size_usd),
                time_str,
            ])
            .style(Style::default().fg(color))
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Length(10),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(format!(
            "Live Positions ({} open)",
            state.positions.len()
        )));

    f.render_widget(table, area);
}

// ── TAB 3: Signal Feed ────────────────────────────────────────────────────────

fn draw_signals(f: &mut Frame, area: Rect, state: &DashboardState) {
    let items: Vec<ListItem> = state
        .signal_log
        .iter()
        .rev()
        .take(area.height as usize)
        .map(|e| {
            let color = match e.action.as_str() {
                "TRADED" => Color::Green,
                "REJECTED" => Color::Red,
                _ => Color::DarkGray,
            };
            let groq = e.groq_score.map(|s| s.to_string()).unwrap_or_else(|| "--".to_string());
            let claude = e.claude_score.map(|s| s.to_string()).unwrap_or_else(|| "--".to_string());
            let kelly = e.kelly_size.map(|s| format!("${s:.2}")).unwrap_or_else(|| "--".to_string());
            let text = format!(
                "[{}] {:<20} | G:{:>3} C:{:>3} | Kelly:{:>6} | {}",
                e.time, e.market_slug, groq, claude, kelly, e.action
            );
            ListItem::new(text).style(Style::default().fg(color))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Signal Feed ({} total)", state.signal_log.len())),
    );
    f.render_widget(list, area);
}

// ── TAB 4: Risk Panel ─────────────────────────────────────────────────────────

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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    // Drawdown gauge.
    let dd_f64: f64 = dd_pct.to_string().parse().unwrap_or(0.0);
    let dd_color = if dd_f64 >= 0.15 { Color::Red } else if dd_f64 >= 0.05 { Color::Yellow } else { Color::Green };
    let halt_label = if halted { " *** HALT ***" } else { "" };
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(format!(
            "Drawdown  peak=${peak:.2}  current=${current_bk:.2}{halt_label}"
        )))
        .gauge_style(Style::default().fg(dd_color))
        .ratio(dd_f64.min(1.0))
        .label(format!("{:.1}%", dd_f64 * 100.0));
    f.render_widget(gauge, chunks[0]);

    // Regime badge + exposure.
    let open_usd: Decimal = state.positions.iter().map(|p| p.size_usd).sum();
    let regime_str = format!(
        "Regime: {:?}  |  Open exposure: ${:.2} / $40.00  |  Positions: {}",
        regime,
        open_usd,
        state.positions.len()
    );
    let regime_color = match regime {
        Regime::Quiet => Color::Green,
        Regime::Trending => Color::Cyan,
        Regime::Volatile => Color::Yellow,
        Regime::Breaking => Color::Red,
    };
    let regime_para = Paragraph::new(regime_str)
        .block(Block::default().borders(Borders::ALL).title("Regime & Exposure"))
        .style(Style::default().fg(regime_color));
    f.render_widget(regime_para, chunks[1]);

    // Kelly bet size bar chart (last 20 bets).
    if !state.kelly_history.is_empty() {
        let data: Vec<(&str, u64)> = state
            .kelly_history
            .iter()
            .map(|v| {
                let cents = (v.to_string().parse::<f64>().unwrap_or(0.0) * 100.0) as u64;
                ("", cents)
            })
            .collect();
        let bar_chart = BarChart::default()
            .block(Block::default().borders(Borders::ALL).title("Kelly Bet Sizes (last 20, in cents)"))
            .data(&data)
            .bar_width(3)
            .bar_gap(1)
            .bar_style(Style::default().fg(Color::Cyan))
            .value_style(Style::default().fg(Color::White));
        f.render_widget(bar_chart, chunks[2]);
    } else {
        let para = Paragraph::new("No bets placed yet.")
            .block(Block::default().borders(Borders::ALL).title("Kelly Bet Sizes"));
        f.render_widget(para, chunks[2]);
    }
}

// ── TAB 5: Analytics ──────────────────────────────────────────────────────────

fn draw_analytics(f: &mut Frame, area: Rect, state: &DashboardState) {
    let flash_wr = if state.flash_total > 0 {
        format!("{:.1}%", state.flash_wins as f64 / state.flash_total as f64 * 100.0)
    } else {
        "N/A".to_string()
    };
    let std_wr = if state.standard_total > 0 {
        format!("{:.1}%", state.standard_wins as f64 / state.standard_total as f64 * 100.0)
    } else {
        "N/A".to_string()
    };

    let text = vec![
        Line::from(format!("Total trades:    {}", state.total_trades)),
        Line::from(format!("Win rate:        {:.1}%", state.win_rate())),
        Line::from(format!("Flash win rate:  {} ({}/{})", flash_wr, state.flash_wins, state.flash_total)),
        Line::from(format!("Std win rate:    {} ({}/{})", std_wr, state.standard_wins, state.standard_total)),
        Line::from(""),
        Line::from(format!("Profit factor:   {:.2}", state.profit_factor())),
        Line::from(format!("Gross wins:      ${:.2}", state.gross_wins)),
        Line::from(format!("Gross losses:    ${:.2}", state.gross_losses)),
        Line::from(""),
        Line::from(format!("Total P&L:       {:+.2}", state.total_pnl)),
        Line::from(format!("Today P&L:       {:+.2}", state.today_pnl)),
    ];

    let para = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title("Analytics"));
    f.render_widget(para, area);
}

// ── TAB 6: AI Advisor ────────────────────────────────────────────────────────

fn draw_ai_advisor(f: &mut Frame, area: Rect, state: &DashboardState, regime: &Regime) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let regime_plain = match regime {
        Regime::Quiet => "Market is quiet. Low volume, tight spreads. Normal sizing.",
        Regime::Trending => "Both feeds agree on direction. Slight size boost active (1.2x).",
        Regime::Volatile => "Large price swings detected. Sizes reduced (0.4x). Flash threshold raised.",
        Regime::Breaking => "Volume spike detected — breaking news likely. Flash paused. Re-scoring positions.",
    };

    let groq_text = format!(
        "Groq live commentary:\n\n{}\n\nRegime: {regime:?}\n\n{regime_plain}",
        state.groq_commentary,
    );
    let groq_para = Paragraph::new(groq_text)
        .block(Block::default().borders(Borders::ALL).title("Groq (Fast Intel)"))
        .wrap(ratatui::widgets::Wrap { trim: true });
    f.render_widget(groq_para, chunks[0]);

    let claude_text = format!(
        "Last Claude deep-verify:\n\n{}",
        state.claude_reasoning,
    );
    let claude_para = Paragraph::new(claude_text)
        .block(Block::default().borders(Borders::ALL).title("Claude (Deep Verify)"))
        .wrap(ratatui::widgets::Wrap { trim: true });
    f.render_widget(claude_para, chunks[1]);
}
