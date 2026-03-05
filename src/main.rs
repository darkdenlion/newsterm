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
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Tabs},
    Frame, Terminal,
};
use rss::Channel;
use std::io;
use textwrap::wrap;

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
        accent: Color::Red,
    },
    FeedSource {
        name: "CNBC",
        url: "https://search.cnbc.com/rs/search/combinedcms/view.xml?partnerId=wrss01&id=100003114",
        accent: Color::Rgb(0, 136, 204),
    },
];

// ── Article model ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Article {
    title: String,
    description: String,
    link: String,
    pub_date: String,
    source: String,
}

// ── App state ─────────────────────────────────────────────────────────────────

enum View {
    List,
    Detail,
}

struct App {
    feed_index: usize,
    articles: Vec<Article>,
    list_state: ListState,
    view: View,
    scroll_offset: u16,
    loading: bool,
    error: Option<String>,
}

impl App {
    fn new() -> Self {
        Self {
            feed_index: 0,
            articles: Vec::new(),
            list_state: ListState::default(),
            view: View::List,
            scroll_offset: 0,
            loading: true,
            error: None,
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

    fn next_feed(&mut self) {
        self.feed_index = (self.feed_index + 1) % FEEDS.len();
    }

    fn prev_feed(&mut self) {
        self.feed_index = if self.feed_index == 0 {
            FEEDS.len() - 1
        } else {
            self.feed_index - 1
        };
    }
}

// ── Fetch RSS ─────────────────────────────────────────────────────────────────

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
            Article {
                title,
                description,
                link,
                pub_date,
                source: source.name.to_string(),
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
    let accent = FEEDS[app.feed_index].accent;

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header / tabs
            Constraint::Min(0),   // body
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    // ── Header tabs ───────────────────────────────────────────────────────
    let tab_titles: Vec<Line> = FEEDS
        .iter()
        .map(|s| Line::from(format!(" {} ", s.name)))
        .collect();

    let tabs = Tabs::new(tab_titles)
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " newsterm ",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ))
                .title_alignment(ratatui::layout::Alignment::Left),
        )
        .select(app.feed_index)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(accent)
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::UNDERLINED),
        )
        .divider(Span::raw("│"));

    f.render_widget(tabs, outer[0]);

    // ── Body ──────────────────────────────────────────────────────────────
    if app.loading {
        let loading = Paragraph::new("  Loading feed...")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().padding(Padding::new(0, 0, 1, 0)));
        f.render_widget(loading, outer[1]);
    } else if let Some(ref err) = app.error {
        let error = Paragraph::new(format!("  Error: {err}"))
            .style(Style::default().fg(Color::Red))
            .block(Block::default().padding(Padding::new(0, 0, 1, 0)));
        f.render_widget(error, outer[1]);
    } else {
        match app.view {
            View::List => render_list(f, app, outer[1], accent),
            View::Detail => render_detail(f, app, outer[1], accent),
        }
    }

    // ── Footer ────────────────────────────────────────────────────────────
    let help = match app.view {
        View::List => " ←/→ switch feed  ↑/↓/j/k navigate  Enter read  q quit ",
        View::Detail => " Esc back  ↑/↓/j/k scroll  o open in browser  q quit ",
    };
    let footer = Paragraph::new(help)
        .style(Style::default().fg(Color::DarkGray).bg(Color::Rgb(30, 30, 30)));
    f.render_widget(footer, outer[2]);
}

fn render_list(f: &mut Frame, app: &App, area: Rect, _accent: Color) {
    let items: Vec<ListItem> = app
        .articles
        .iter()
        .enumerate()
        .map(|(i, article)| {
            let is_selected = app.list_state.selected() == Some(i);
            let date_str = short_date(&article.pub_date);

            let title_style = if is_selected {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Rgb(200, 200, 200))
            };

            let line = Line::from(vec![
                Span::styled(
                    format!("{:>2}  ", i + 1),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(&article.title, title_style),
                Span::styled(
                    format!("  {date_str}"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);

            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::NONE)
                .padding(Padding::new(1, 1, 1, 0)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 40, 50))
                .fg(Color::White),
        )
        .highlight_symbol("▌ ");

    f.render_stateful_widget(list, area, &mut app.list_state.clone());
}

fn render_detail(f: &mut Frame, app: &App, area: Rect, accent: Color) {
    let article = match app.selected_article() {
        Some(a) => a,
        None => return,
    };

    let inner = area.inner(Margin {
        vertical: 1,
        horizontal: 3,
    });

    f.render_widget(Clear, area);

    let content_width = inner.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();

    // Source badge
    lines.push(Line::from(Span::styled(
        format!(" {} ", article.source),
        Style::default()
            .fg(Color::White)
            .bg(accent)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

    // Title
    let title_wrapped = wrap(&article.title, content_width);
    for l in &title_wrapped {
        lines.push(Line::from(Span::styled(
            l.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::default());

    // Date
    if !article.pub_date.is_empty() {
        lines.push(Line::from(Span::styled(
            &article.pub_date,
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::default());
    }

    // Separator
    lines.push(Line::from(Span::styled(
        "─".repeat(content_width.min(60)),
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::default());

    // Description
    let desc_wrapped = wrap(&article.description, content_width.min(80));
    for l in &desc_wrapped {
        lines.push(Line::from(Span::styled(
            l.to_string(),
            Style::default().fg(Color::Rgb(180, 180, 180)),
        )));
    }
    lines.push(Line::default());

    // Link
    if !article.link.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled("Link: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                &article.link,
                Style::default()
                    .fg(accent)
                    .add_modifier(Modifier::UNDERLINED),
            ),
        ]));
    }

    let paragraph = Paragraph::new(Text::from(lines))
        .scroll((app.scroll_offset, 0))
        .block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(accent))
                .padding(Padding::new(1, 0, 0, 0)),
        );

    f.render_widget(paragraph, inner);
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

    // Initial fetch
    let mut current_feed = app.feed_index;
    match fetch_feed(&FEEDS[current_feed]).await {
        Ok(articles) => {
            app.articles = articles;
            if !app.articles.is_empty() {
                app.list_state.select(Some(0));
            }
        }
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
                        KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
                            app.next_feed();
                        }
                        KeyCode::Left | KeyCode::Char('h') | KeyCode::BackTab => {
                            app.prev_feed();
                        }
                        KeyCode::Enter => {
                            if app.selected_article().is_some() {
                                app.scroll_offset = 0;
                                app.view = View::Detail;
                            }
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

        // Re-fetch if feed tab changed
        if app.feed_index != current_feed {
            current_feed = app.feed_index;
            app.loading = true;
            app.error = None;
            app.articles.clear();
            app.list_state.select(None);
            app.view = View::List;

            terminal.draw(|f| ui(f, &app))?;

            match fetch_feed(&FEEDS[current_feed]).await {
                Ok(articles) => {
                    app.articles = articles;
                    if !app.articles.is_empty() {
                        app.list_state.select(Some(0));
                    }
                }
                Err(e) => app.error = Some(e),
            }
            app.loading = false;
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
