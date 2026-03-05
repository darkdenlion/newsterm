use chrono::{DateTime, FixedOffset, Utc};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use html_escape::decode_html_entities;
use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph},
    Frame, Terminal,
};
use rss::Channel;
use std::io;
use textwrap::wrap;

// ── Theme ─────────────────────────────────────────────────────────────────────

mod theme {
    use ratatui::style::Color;

    pub const FG: Color = Color::Rgb(200, 200, 200);
    pub const FG_DIM: Color = Color::Rgb(100, 100, 110);
    pub const FG_MUTED: Color = Color::Rgb(140, 140, 150);
    pub const HIGHLIGHT_BG: Color = Color::Rgb(35, 35, 50);
    pub const BORDER: Color = Color::Rgb(50, 50, 65);
    pub const BREAKING_BG: Color = Color::Rgb(40, 20, 20);
    pub const BREAKING_ACCENT: Color = Color::Rgb(255, 80, 80);
    pub const FOOTER_BG: Color = Color::Rgb(25, 25, 35);

    pub const CNN: Color = Color::Rgb(204, 50, 50);
    pub const CNBC: Color = Color::Rgb(0, 136, 204);
}

// ── Feed sources ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct FeedSource {
    name: &'static str,
    url: &'static str,
    accent: Color,
}

const FEEDS: &[FeedSource] = &[
    FeedSource {
        name: "CNN",
        url: "http://rss.cnn.com/rss/edition.rss",
        accent: theme::CNN,
    },
    FeedSource {
        name: "CNBC",
        url: "https://search.cnbc.com/rs/search/combinedcms/view.xml?partnerId=wrss01&id=100003114",
        accent: theme::CNBC,
    },
];

const BREAKING_COUNT: usize = 3;

// ── Article model ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Article {
    title: String,
    description: String,
    link: String,
    pub_date: String,
    parsed_date: Option<DateTime<FixedOffset>>,
    source: String,
    source_color: Color,
}

impl Article {
    fn age_label(&self) -> String {
        let Some(date) = self.parsed_date else {
            return String::new();
        };
        let now = Utc::now();
        let diff = now.signed_duration_since(date);

        if diff.num_minutes() < 1 {
            "just now".to_string()
        } else if diff.num_minutes() < 60 {
            format!("{}m ago", diff.num_minutes())
        } else if diff.num_hours() < 24 {
            format!("{}h ago", diff.num_hours())
        } else if diff.num_days() < 7 {
            format!("{}d ago", diff.num_days())
        } else {
            short_date(&self.pub_date)
        }
    }
}

// ── App state ─────────────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum View {
    List,
    Detail,
}

struct App {
    breaking: Vec<Article>,
    articles: Vec<Article>,
    list_state: ListState,
    view: View,
    scroll_offset: u16,
    loading: bool,
    error: Option<String>,
    last_updated: Option<DateTime<Utc>>,
}

impl App {
    fn new() -> Self {
        Self {
            breaking: Vec::new(),
            articles: Vec::new(),
            list_state: ListState::default(),
            view: View::List,
            scroll_offset: 0,
            loading: true,
            error: None,
            last_updated: None,
        }
    }

    fn selected_article(&self) -> Option<&Article> {
        self.list_state.selected().and_then(|i| self.articles.get(i))
    }

    fn next_article(&mut self) {
        if self.articles.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => (i + 1).min(self.articles.len() - 1),
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn prev_article(&mut self) {
        if self.articles.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn populate(&mut self, mut all: Vec<Article>) {
        // Sort newest first
        all.sort_by(|a, b| b.parsed_date.cmp(&a.parsed_date));

        // Top N become breaking news
        self.breaking = all.iter().take(BREAKING_COUNT).cloned().collect();

        // Rest go into the main list
        self.articles = all.into_iter().skip(BREAKING_COUNT).collect();

        if !self.articles.is_empty() {
            self.list_state.select(Some(0));
        }
        self.last_updated = Some(Utc::now());
    }
}

// ── Fetch RSS ─────────────────────────────────────────────────────────────────

async fn fetch_all_feeds() -> Result<Vec<Article>, String> {
    let mut all = Vec::new();

    for source in FEEDS {
        match fetch_feed(source).await {
            Ok(articles) => all.extend(articles),
            Err(e) => return Err(format!("{}: {e}", source.name)),
        }
    }

    Ok(all)
}

async fn fetch_feed(source: &FeedSource) -> Result<Vec<Article>, String> {
    let content = reqwest::get(source.url)
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("Read failed: {e}"))?;

    let channel =
        Channel::read_from(&content[..]).map_err(|e| format!("Parse failed: {e}"))?;

    let articles = channel
        .items()
        .iter()
        .map(|item| {
            let title = decode_html_entities(item.title().unwrap_or("Untitled")).to_string();
            let description =
                decode_html_entities(item.description().unwrap_or("No description available."))
                    .to_string();
            let description = strip_html(&description);
            let link = item.link().unwrap_or("").to_string();
            let pub_date = item.pub_date().unwrap_or("").to_string();
            let parsed_date = DateTime::parse_from_rfc2822(&pub_date).ok();
            Article {
                title,
                description,
                link,
                pub_date,
                parsed_date,
                source: source.name.to_string(),
                source_color: source.accent,
            }
        })
        .collect();

    Ok(articles)
}

fn strip_html(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    output.trim().to_string()
}

// ── UI rendering ──────────────────────────────────────────────────────────────

fn ui(f: &mut Frame, app: &App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),                                        // header
            Constraint::Length((BREAKING_COUNT as u16).saturating_add(3)), // breaking
            Constraint::Min(0),                                          // main list
            Constraint::Length(1),                                        // footer
        ])
        .split(f.area());

    render_header(f, app, outer[0]);

    if app.loading {
        let loading = Paragraph::new("  Fetching feeds from CNN & CNBC...")
            .style(Style::default().fg(theme::FG_DIM));
        let area = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(0)])
            .split(outer[2])[1];
        f.render_widget(loading, area);
    } else if let Some(ref err) = app.error {
        let error = Paragraph::new(format!("  Error: {err}"))
            .style(Style::default().fg(Color::Red));
        f.render_widget(error, outer[2]);
    } else {
        match app.view {
            View::List => {
                render_breaking(f, app, outer[1]);
                render_list(f, app, outer[2]);
            }
            View::Detail => {
                render_detail(f, app, outer[1], outer[2]);
            }
        }
    }

    render_footer(f, app, outer[3]);
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let updated_text = if let Some(time) = app.last_updated {
        let ago = Utc::now().signed_duration_since(time);
        if ago.num_seconds() < 60 {
            "Updated just now".to_string()
        } else {
            format!("Updated {}m ago", ago.num_minutes())
        }
    } else {
        String::new()
    };

    let title_line = Line::from(vec![
        Span::styled(
            " newsterm ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "│ ",
            Style::default().fg(theme::BORDER),
        ),
        Span::styled(
            "CNN",
            Style::default()
                .fg(theme::CNN)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" + ", Style::default().fg(theme::FG_DIM)),
        Span::styled(
            "CNBC",
            Style::default()
                .fg(theme::CNBC)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let right_text = Line::from(Span::styled(
        format!("{updated_text} "),
        Style::default().fg(theme::FG_DIM),
    ));

    let header_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(updated_text.len() as u16 + 2)])
        .split(area);

    let left = Paragraph::new(title_line).block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(theme::BORDER))
            .padding(Padding::new(0, 0, 1, 0)),
    );

    let right = Paragraph::new(right_text)
        .alignment(ratatui::layout::Alignment::Right)
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(theme::BORDER))
                .padding(Padding::new(0, 0, 1, 0)),
        );

    f.render_widget(left, header_layout[0]);
    f.render_widget(right, header_layout[1]);
}

fn render_breaking(f: &mut Frame, app: &App, area: Rect) {
    if app.breaking.is_empty() {
        return;
    }

    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(theme::BORDER))
        .padding(Padding::new(1, 1, 0, 0));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            std::iter::once(Constraint::Length(1)) // section title
                .chain(app.breaking.iter().map(|_| Constraint::Length(1)))
                .collect::<Vec<_>>(),
        )
        .split(inner);

    // Section title
    let title = Line::from(vec![
        Span::styled(
            " ▲ ",
            Style::default()
                .fg(Color::White)
                .bg(theme::BREAKING_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " BREAKING",
            Style::default()
                .fg(theme::BREAKING_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(Paragraph::new(title), rows[0]);

    // Breaking headlines
    for (i, article) in app.breaking.iter().enumerate() {
        let age = article.age_label();
        let line = Line::from(vec![
            Span::styled("  ", Style::default()),
            source_badge(&article.source, article.source_color),
            Span::styled(" ", Style::default()),
            Span::styled(
                &article.title,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {age}"),
                Style::default().fg(theme::FG_DIM),
            ),
        ]);
        let bg = if i % 2 == 0 {
            theme::BREAKING_BG
        } else {
            Color::Reset
        };
        let p = Paragraph::new(line).style(Style::default().bg(bg));
        f.render_widget(p, rows[i + 1]);
    }
}

fn render_list(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::NONE)
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Section title row + list
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(inner);

    let section_title = Line::from(vec![
        Span::styled(
            " LATEST NEWS ",
            Style::default()
                .fg(theme::FG_MUTED)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} articles", app.articles.len()),
            Style::default().fg(theme::FG_DIM),
        ),
    ]);
    f.render_widget(
        Paragraph::new(section_title).block(Block::default().padding(Padding::new(0, 0, 0, 0))),
        layout[0],
    );

    let items: Vec<ListItem> = app
        .articles
        .iter()
        .map(|article| {
            let age = article.age_label();
            let line = Line::from(vec![
                Span::styled(" ", Style::default()),
                source_badge(&article.source, article.source_color),
                Span::styled("  ", Style::default()),
                Span::styled(
                    &article.title,
                    Style::default().fg(theme::FG),
                ),
                Span::styled(
                    format!("  {age}"),
                    Style::default().fg(theme::FG_DIM),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(theme::HIGHLIGHT_BG)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" ▌");

    f.render_stateful_widget(list, layout[1], &mut app.list_state.clone());
}

fn render_detail(f: &mut Frame, app: &App, breaking_area: Rect, list_area: Rect) {
    let article = match app.selected_article() {
        Some(a) => a,
        None => return,
    };

    // Use the full space (breaking + list area)
    let full = Rect {
        x: breaking_area.x,
        y: breaking_area.y,
        width: breaking_area.width,
        height: breaking_area.height + list_area.height,
    };

    f.render_widget(Clear, full);

    let inner = full.inner(Margin {
        vertical: 1,
        horizontal: 4,
    });

    let content_width = inner.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();

    // Source badge
    lines.push(Line::from(vec![
        source_badge(&article.source, article.source_color),
        Span::styled(
            format!("  {}", article.age_label()),
            Style::default().fg(theme::FG_DIM),
        ),
    ]));
    lines.push(Line::default());

    // Title
    if content_width > 0 {
        let title_wrapped = wrap(&article.title, content_width);
        for l in &title_wrapped {
            lines.push(Line::from(Span::styled(
                l.to_string(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )));
        }
    }
    lines.push(Line::default());

    // Full date
    if !article.pub_date.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {}", article.pub_date),
            Style::default().fg(theme::FG_DIM),
        )));
        lines.push(Line::default());
    }

    // Separator
    lines.push(Line::from(Span::styled(
        "─".repeat(content_width.min(60)),
        Style::default().fg(theme::BORDER),
    )));
    lines.push(Line::default());

    // Description
    let wrap_width = content_width.min(80);
    if wrap_width > 0 {
        let desc_wrapped = wrap(&article.description, wrap_width);
        for l in &desc_wrapped {
            lines.push(Line::from(Span::styled(
                l.to_string(),
                Style::default().fg(theme::FG),
            )));
        }
    }

    // Link
    if !article.link.is_empty() {
        lines.push(Line::default());
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled("  Link  ", Style::default().fg(theme::FG_DIM)),
            Span::styled(
                &article.link,
                Style::default()
                    .fg(article.source_color)
                    .add_modifier(Modifier::UNDERLINED),
            ),
        ]));
    }

    let paragraph = Paragraph::new(Text::from(lines))
        .scroll((app.scroll_offset, 0))
        .block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(article.source_color))
                .padding(Padding::new(1, 0, 0, 0)),
        );

    f.render_widget(paragraph, inner);
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let help = match app.view {
        View::List => vec![
            Span::styled(" ↑/↓ j/k ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("navigate  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("Enter ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("read  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("r ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("refresh  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("q ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("quit", Style::default().fg(theme::FG_DIM)),
        ],
        View::Detail => vec![
            Span::styled(" Esc ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("back  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("↑/↓ j/k ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("scroll  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("o ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("open in browser  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("q ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("quit", Style::default().fg(theme::FG_DIM)),
        ],
    };

    let footer = Paragraph::new(Line::from(help))
        .style(Style::default().bg(theme::FOOTER_BG));
    f.render_widget(footer, area);
}

fn source_badge(name: &str, color: Color) -> Span<'_> {
    Span::styled(
        format!(" {name} "),
        Style::default()
            .fg(Color::White)
            .bg(color)
            .add_modifier(Modifier::BOLD),
    )
}

fn short_date(date: &str) -> String {
    let parts: Vec<&str> = date.split_whitespace().collect();
    if parts.len() >= 4 {
        format!("{} {}", parts[2], parts[1])
    } else {
        date.to_string()
    }
}

// ── Main loop ─────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();

    // Initial fetch — all feeds combined
    match fetch_all_feeds().await {
        Ok(articles) => app.populate(articles),
        Err(e) => app.error = Some(e),
    }
    app.loading = false;

    loop {
        terminal.draw(|f| ui(f, &app))?;

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                match app.view {
                    View::List => match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Down | KeyCode::Char('j') => app.next_article(),
                        KeyCode::Up | KeyCode::Char('k') => app.prev_article(),
                        KeyCode::Enter => {
                            if app.selected_article().is_some() {
                                app.scroll_offset = 0;
                                app.view = View::Detail;
                            }
                        }
                        KeyCode::Char('r') => {
                            app.loading = true;
                            app.error = None;
                            terminal.draw(|f| ui(f, &app))?;

                            match fetch_all_feeds().await {
                                Ok(articles) => app.populate(articles),
                                Err(e) => app.error = Some(e),
                            }
                            app.loading = false;
                        }
                        _ => {}
                    },
                    View::Detail => match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Esc | KeyCode::Backspace => {
                            app.view = View::List;
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            app.scroll_offset = app.scroll_offset.saturating_add(1);
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.scroll_offset = app.scroll_offset.saturating_sub(1);
                        }
                        KeyCode::Char('o') => {
                            if let Some(article) = app.selected_article() {
                                if !article.link.is_empty() {
                                    let _ = open::that(&article.link);
                                }
                            }
                        }
                        _ => {}
                    },
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
