use std::collections::VecDeque;
use std::error::Error;
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand};
use rand::rngs::ThreadRng;
use rand::Rng;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Cell, Chart, Dataset, List, ListItem, Paragraph, Row, Table, Wrap,
};
use ratatui::{Frame, Terminal};

const TICK_RATE: Duration = Duration::from_millis(180);
const MAX_HISTORY: usize = 120;
const MAX_TRADES: usize = 12;
const MAX_LOG_LINES: usize = 18;

fn main() -> Result<(), Box<dyn Error>> {
    let mut terminal = setup_terminal()?;
    let result = run_app(&mut terminal);
    restore_terminal(&mut terminal)?;
    result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>, Box<dyn Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<(), Box<dyn Error>> {
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<(), Box<dyn Error>> {
    let mut app = App::new();
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|frame| draw(frame, &app))?;

        let timeout = TICK_RATE.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if !app.handle_key(key.code) {
                    break;
                }
            }
        }

        if last_tick.elapsed() >= TICK_RATE {
            app.on_tick();
            last_tick = Instant::now();
        }
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum Side {
    Buy,
    Sell,
}

impl Side {
    fn sign(self) -> f64 {
        match self {
            Side::Buy => 1.0,
            Side::Sell => -1.0,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        }
    }

    fn color(self) -> Color {
        match self {
            Side::Buy => Color::Green,
            Side::Sell => Color::Red,
        }
    }
}

#[derive(Clone)]
struct Instrument {
    symbol: &'static str,
    price: f64,
    drift: f64,
    volatility: f64,
    day_open: f64,
    history: VecDeque<(f64, f64)>,
}

impl Instrument {
    fn new(symbol: &'static str, price: f64, drift: f64, volatility: f64) -> Self {
        let mut history = VecDeque::new();
        for idx in 0..MAX_HISTORY {
            history.push_back((idx as f64, price));
        }

        Self {
            symbol,
            price,
            drift,
            volatility,
            day_open: price,
            history,
        }
    }

    fn change_percent(&self) -> f64 {
        ((self.price - self.day_open) / self.day_open) * 100.0
    }
}

struct Position {
    qty: f64,
    average_price: f64,
    realized_pnl: f64,
}

impl Position {
    fn new() -> Self {
        Self {
            qty: 0.0,
            average_price: 0.0,
            realized_pnl: 0.0,
        }
    }

    fn unrealized_pnl(&self, mark: f64) -> f64 {
        if self.qty.abs() < f64::EPSILON {
            0.0
        } else {
            (mark - self.average_price) * self.qty
        }
    }

    fn exposure(&self, mark: f64) -> f64 {
        self.qty.abs() * mark
    }

    fn apply_fill(&mut self, side: Side, qty: f64, price: f64) -> f64 {
        let signed_qty = qty * side.sign();
        let old_qty = self.qty;
        let new_qty = old_qty + signed_qty;
        let mut realized = 0.0;

        if old_qty.abs() < f64::EPSILON || old_qty.signum() == signed_qty.signum() {
            let total_cost = (self.average_price * old_qty.abs()) + (price * qty);
            self.qty = new_qty;
            self.average_price = if self.qty.abs() < f64::EPSILON {
                0.0
            } else {
                total_cost / self.qty.abs()
            };
            return 0.0;
        }

        let closing_qty = old_qty.abs().min(qty);
        realized = if old_qty > 0.0 {
            (price - self.average_price) * closing_qty
        } else {
            (self.average_price - price) * closing_qty
        };

        self.realized_pnl += realized;
        self.qty = new_qty;

        if self.qty.abs() < f64::EPSILON {
            self.qty = 0.0;
            self.average_price = 0.0;
        } else if old_qty.signum() != self.qty.signum() {
            self.average_price = price;
        }

        realized
    }
}

struct Trade {
    time_index: u64,
    symbol: &'static str,
    side: Side,
    qty: f64,
    price: f64,
    realized: f64,
}

struct OrderTicket {
    qty: f64,
}

impl OrderTicket {
    fn new() -> Self {
        Self { qty: 1.0 }
    }

    fn increase(&mut self) {
        self.qty = (self.qty + 1.0).min(250.0);
    }

    fn decrease(&mut self) {
        self.qty = (self.qty - 1.0).max(1.0);
    }
}

struct App {
    instruments: Vec<Instrument>,
    positions: Vec<Position>,
    trades: VecDeque<Trade>,
    log: VecDeque<String>,
    ticket: OrderTicket,
    selected: usize,
    clock: u64,
    rng: ThreadRng,
}

impl App {
    fn new() -> Self {
        let instruments = vec![
            Instrument::new("BTCUSDT", 64_250.0, 0.0018, 0.012),
            Instrument::new("ETHUSDT", 3_180.0, 0.0013, 0.010),
            Instrument::new("SOLUSDT", 142.0, 0.0010, 0.017),
            Instrument::new("AAPL", 201.4, 0.0004, 0.004),
        ];

        let positions = (0..instruments.len()).map(|_| Position::new()).collect();

        let mut log = VecDeque::new();
        log.push_front(String::from(
            "Terminal started. Press b/s to trade, Tab to switch instrument.",
        ));

        Self {
            instruments,
            positions,
            trades: VecDeque::new(),
            log,
            ticket: OrderTicket::new(),
            selected: 0,
            clock: 0,
            rng: rand::thread_rng(),
        }
    }

    fn handle_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Tab | KeyCode::Right => self.selected = (self.selected + 1) % self.instruments.len(),
            KeyCode::BackTab | KeyCode::Left => {
                self.selected = if self.selected == 0 {
                    self.instruments.len() - 1
                } else {
                    self.selected - 1
                }
            }
            KeyCode::Up | KeyCode::Char('+') => self.ticket.increase(),
            KeyCode::Down | KeyCode::Char('-') => self.ticket.decrease(),
            KeyCode::Char('b') => self.place_market_order(Side::Buy),
            KeyCode::Char('s') => self.place_market_order(Side::Sell),
            KeyCode::Char('c') => self.close_position(),
            _ => {}
        }
        true
    }

    fn on_tick(&mut self) {
        self.clock += 1;

        for instrument in &mut self.instruments {
            let noise = self.rng.gen_range(-instrument.volatility..instrument.volatility);
            let next_price = instrument.price * (1.0 + instrument.drift + noise);
            instrument.price = next_price.max(0.1);
            if instrument.history.len() >= MAX_HISTORY {
                instrument.history.pop_front();
            }
            instrument
                .history
                .push_back((self.clock as f64, instrument.price));
        }

        if self.clock % 8 == 0 {
            let symbol = self.current_instrument().symbol;
            let price = self.current_instrument().price;
            self.push_log(format!("MARK {} {:.2}", symbol, price));
        }
    }

    fn current_instrument(&self) -> &Instrument {
        &self.instruments[self.selected]
    }

    fn place_market_order(&mut self, side: Side) {
        let symbol = self.instruments[self.selected].symbol;
        let mark_price = self.instruments[self.selected].price;
        let slippage = self.rng.gen_range(0.0..(mark_price * 0.0015));
        let price = match side {
            Side::Buy => mark_price + slippage,
            Side::Sell => mark_price - slippage,
        };
        let qty = self.ticket.qty;
        let (realized, position_qty, realized_pnl) = {
            let position = &mut self.positions[self.selected];
            let realized = position.apply_fill(side, qty, price);
            (realized, position.qty, position.realized_pnl)
        };

        if self.trades.len() >= MAX_TRADES {
            self.trades.pop_back();
        }
        self.trades.push_front(Trade {
            time_index: self.clock,
            symbol,
            side,
            qty,
            price,
            realized,
        });

        self.push_log(format!(
            "{} {} {:.0} @ {:.2} | pos {:.0} | rPNL {:+.2}",
            side.label(),
            symbol,
            qty,
            price,
            position_qty,
            realized_pnl
        ));
    }

    fn close_position(&mut self) {
        let qty = self.positions[self.selected].qty;
        if qty.abs() < f64::EPSILON {
            self.push_log(format!(
                "Position in {} is already flat.",
                self.current_instrument().symbol
            ));
            return;
        }

        let side = if qty > 0.0 { Side::Sell } else { Side::Buy };
        let previous_qty = self.ticket.qty;
        self.ticket.qty = qty.abs();
        self.place_market_order(side);
        self.ticket.qty = previous_qty;
    }

    fn push_log(&mut self, entry: String) {
        if self.log.len() >= MAX_LOG_LINES {
            self.log.pop_back();
        }
        self.log.push_front(entry);
    }

    fn total_unrealized(&self) -> f64 {
        self.positions
            .iter()
            .zip(self.instruments.iter())
            .map(|(position, instrument)| position.unrealized_pnl(instrument.price))
            .sum()
    }

    fn total_realized(&self) -> f64 {
        self.positions.iter().map(|position| position.realized_pnl).sum()
    }

    fn gross_exposure(&self) -> f64 {
        self.positions
            .iter()
            .zip(self.instruments.iter())
            .map(|(position, instrument)| position.exposure(instrument.price))
            .sum()
    }
}

fn draw(frame: &mut Frame, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(16),
            Constraint::Length(9),
        ])
        .split(frame.area());

    draw_header(frame, root[0], app);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(34),
            Constraint::Min(50),
            Constraint::Length(42),
        ])
        .split(root[1]);

    draw_watchlist(frame, main[0], app);
    draw_chart(frame, main[1], app);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Min(8),
        ])
        .split(main[2]);

    draw_ticket(frame, right[0], app);
    draw_positions(frame, right[1], app);
    draw_trades(frame, right[2], app);

    draw_log(frame, root[2], app);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let realized = app.total_realized();
    let unrealized = app.total_unrealized();
    let exposure = app.gross_exposure();
    let selected = app.current_instrument();
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " Rust Trading Terminal ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{} {:.2}", selected.symbol, selected.price),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("d% {:+.2}", selected.change_percent()),
            Style::default().fg(pnl_color(selected.change_percent())),
        ),
        Span::raw("  "),
        Span::styled(
            format!("rPNL {:+.2}", realized),
            Style::default().fg(pnl_color(realized)),
        ),
        Span::raw("  "),
        Span::styled(
            format!("uPNL {:+.2}", unrealized),
            Style::default().fg(pnl_color(unrealized)),
        ),
        Span::raw("  "),
        Span::styled(
            format!("gross {:.2}", exposure),
            Style::default().fg(Color::Yellow),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL).title("Overview"))
    .alignment(Alignment::Left);

    frame.render_widget(header, area);
}

fn draw_watchlist(frame: &mut Frame, area: Rect, app: &App) {
    let rows = app.instruments.iter().enumerate().map(|(idx, instrument)| {
        let style = if idx == app.selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightCyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        Row::new(vec![
            Cell::from(instrument.symbol),
            Cell::from(format!("{:.2}", instrument.price)),
            Cell::from(format!("{:+.2}%", instrument.change_percent())),
        ])
        .style(style)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(vec!["Symbol", "Last", "Change"])
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title("Watchlist"));

    frame.render_widget(table, area);
}

fn draw_chart(frame: &mut Frame, area: Rect, app: &App) {
    let instrument = app.current_instrument();
    let points: Vec<(f64, f64)> = instrument.history.iter().copied().collect();
    let min_price = points
        .iter()
        .map(|(_, value)| *value)
        .fold(f64::INFINITY, f64::min);
    let max_price = points
        .iter()
        .map(|(_, value)| *value)
        .fold(f64::NEG_INFINITY, f64::max);

    let padding = ((max_price - min_price) * 0.15).max(1.0);
    let dataset = Dataset::default()
        .name("mid")
        .marker(symbols::Marker::Braille)
        .style(Style::default().fg(Color::Cyan))
        .data(&points);

    let chart = Chart::new(vec![dataset])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Chart {}", instrument.symbol)),
        )
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([
                    app.clock.saturating_sub(MAX_HISTORY as u64) as f64,
                    app.clock.max(MAX_HISTORY as u64) as f64,
                ])
                .labels(vec![
                    Span::styled("older", Style::default().fg(Color::DarkGray)),
                    Span::styled("now", Style::default().fg(Color::DarkGray)),
                ]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([min_price - padding, max_price + padding])
                .labels(vec![
                    Span::raw(format!("{:.2}", min_price)),
                    Span::raw(format!("{:.2}", instrument.price)),
                    Span::raw(format!("{:.2}", max_price)),
                ]),
        );

    frame.render_widget(chart, area);
}

fn draw_ticket(frame: &mut Frame, area: Rect, app: &App) {
    let instrument = app.current_instrument();
    let bid = instrument.price * 0.9992;
    let ask = instrument.price * 1.0008;
    let position = &app.positions[app.selected];

    let lines = vec![
        Line::from(vec![
            Span::styled("Symbol: ", Style::default().fg(Color::Gray)),
            Span::raw(instrument.symbol),
        ]),
        Line::from(vec![
            Span::styled("Qty:    ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.0}", app.ticket.qty),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Bid:    ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:.2}", bid), Style::default().fg(Color::Green)),
        ]),
        Line::from(vec![
            Span::styled("Ask:    ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:.2}", ask), Style::default().fg(Color::Red)),
        ]),
        Line::from(vec![
            Span::styled("Pos:    ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.0} @ {:.2}", position.qty, position.average_price),
                Style::default().fg(pnl_color(position.qty)),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Order Ticket"))
        .wrap(Wrap { trim: true });

    frame.render_widget(paragraph, area);
}

fn draw_positions(frame: &mut Frame, area: Rect, app: &App) {
    let rows = app
        .positions
        .iter()
        .zip(app.instruments.iter())
        .map(|(position, instrument)| {
            let unrealized = position.unrealized_pnl(instrument.price);
            Row::new(vec![
                Cell::from(instrument.symbol),
                Cell::from(format!("{:.0}", position.qty)),
                Cell::from(format!("{:.2}", position.average_price)),
                Cell::from(format!("{:+.2}", unrealized)),
                Cell::from(format!("{:+.2}", position.realized_pnl)),
            ])
        });

    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(vec!["Symbol", "Qty", "Avg", "uPNL", "rPNL"])
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title("Positions"));

    frame.render_widget(table, area);
}

fn draw_trades(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .trades
        .iter()
        .map(|trade| {
            let line = Line::from(vec![
                Span::styled(
                    format!("[{:03}] ", trade.time_index),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(trade.side.label(), Style::default().fg(trade.side.color())),
                Span::raw(format!(" {} {:.0} @ {:.2}", trade.symbol, trade.qty, trade.price)),
                Span::raw(" "),
                Span::styled(
                    format!("{:+.2}", trade.realized),
                    Style::default().fg(pnl_color(trade.realized)),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(Block::default().borders(Borders::ALL).title("Recent Trades"));
    frame.render_widget(list, area);
}

fn draw_log(frame: &mut Frame, area: Rect, app: &App) {
    let help = "Tab/Left/Right switch | Up/Down qty | b buy | s sell | c close | q quit";
    let mut lines = Vec::with_capacity(app.log.len() + 1);
    lines.push(Line::from(vec![
        Span::styled("Hotkeys: ", Style::default().fg(Color::Cyan)),
        Span::raw(help),
    ]));
    lines.extend(app.log.iter().map(|entry| Line::raw(entry.clone())));

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Execution Log"))
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

fn pnl_color(value: f64) -> Color {
    if value > 0.0 {
        Color::Green
    } else if value < 0.0 {
        Color::Red
    } else {
        Color::Gray
    }
}
