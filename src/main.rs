mod config;
mod store;

use chrono::{DateTime, FixedOffset, Utc};
use cli_clipboard::{ClipboardContext, ClipboardProvider};
use config::{parse_hex_color, Config};
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use html_escape::decode_html_entities;
use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph},
    Frame, Terminal,
};
use rss::Channel;
use std::collections::HashMap;
use std::io;
use store::Store;
use textwrap::wrap;
use tokio::sync::mpsc;

// ── Theme ─────────────────────────────────────────────────────────────────────

mod theme {
    use ratatui::style::Color;

    pub const FG: Color = Color::Rgb(220, 220, 220);
    pub const FG_DIM: Color = Color::Rgb(100, 100, 110);
    pub const FG_MUTED: Color = Color::Rgb(140, 140, 150);
    pub const FG_READ: Color = Color::Rgb(90, 90, 100);
    pub const HIGHLIGHT_BG: Color = Color::Rgb(35, 35, 50);
    pub const BORDER: Color = Color::Rgb(50, 50, 65);
    pub const BREAKING_BG: Color = Color::Rgb(40, 20, 20);
    pub const BREAKING_ACCENT: Color = Color::Rgb(255, 80, 80);
    pub const FOOTER_BG: Color = Color::Rgb(25, 25, 35);
    pub const SPINNER: Color = Color::Rgb(120, 120, 200);
    pub const BOOKMARK: Color = Color::Rgb(255, 200, 50);
    pub const SUCCESS: Color = Color::Rgb(80, 200, 80);
}

// ── Feed sources ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct FeedSource {
    name: String,
    url: String,
    accent: Color,
}

impl FeedSource {
    fn from_config(feeds: &[config::FeedConfig]) -> Vec<Self> {
        feeds
            .iter()
            .map(|f| FeedSource {
                name: f.name.clone(),
                url: f.url.clone(),
                accent: parse_hex_color(&f.color),
            })
            .collect()
    }
}

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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

// ── Background messages ───────────────────────────────────────────────────────

enum AppMessage {
    FeedResult(Vec<Article>),
    FeedError(String),
    PartialError(String),
    ArticleContent { link: String, content: String },
    ArticleFetchError { link: String, error: String },
}

// ── App state ─────────────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum View {
    List,
    Detail,
}

struct App {
    feeds: Vec<FeedSource>,
    breaking_count: usize,
    breaking: Vec<Article>,
    all_articles: Vec<Article>,
    filtered_articles: Vec<Article>,
    list_state: ListState,
    view: View,
    scroll_offset: u16,
    loading: bool,
    error: Option<String>,
    last_updated: Option<DateTime<Utc>>,
    spinner_tick: usize,
    search_active: bool,
    search_query: String,
    store: Store,
    toast: Option<(String, std::time::Instant)>,
    // Article content cache: link -> full text
    content_cache: HashMap<String, String>,
    // Currently loading article content for this link
    loading_article: Option<String>,
    // Breaking news ticker scroll offset
    ticker_offset: usize,
}

impl App {
    fn new(feeds: Vec<FeedSource>, breaking_count: usize) -> Self {
        Self {
            feeds,
            breaking_count,
            breaking: Vec::new(),
            all_articles: Vec::new(),
            filtered_articles: Vec::new(),
            list_state: ListState::default(),
            view: View::List,
            scroll_offset: 0,
            loading: true,
            error: None,
            last_updated: None,
            spinner_tick: 0,
            search_active: false,
            search_query: String::new(),
            store: Store::load(),
            toast: None,
            content_cache: HashMap::new(),
            loading_article: None,
            ticker_offset: 0,
        }
    }

    fn visible_articles(&self) -> &[Article] {
        if self.search_query.is_empty() {
            &self.all_articles
        } else {
            &self.filtered_articles
        }
    }

    fn selected_article(&self) -> Option<&Article> {
        self.list_state
            .selected()
            .and_then(|i| self.visible_articles().get(i))
    }

    fn next_article(&mut self) {
        let len = self.visible_articles().len();
        if len == 0 {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => (i + 1).min(len - 1),
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn prev_article(&mut self) {
        let len = self.visible_articles().len();
        if len == 0 {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn page_down(&mut self, page_size: usize) {
        let len = self.visible_articles().len();
        if len == 0 {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => (i + page_size).min(len - 1),
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn page_up(&mut self, page_size: usize) {
        let len = self.visible_articles().len();
        if len == 0 {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => i.saturating_sub(page_size),
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn apply_filter(&mut self) {
        if self.search_query.is_empty() {
            self.filtered_articles.clear();
        } else {
            let q = self.search_query.to_lowercase();
            self.filtered_articles = self
                .all_articles
                .iter()
                .filter(|a| {
                    a.title.to_lowercase().contains(&q)
                        || a.description.to_lowercase().contains(&q)
                        || a.source.to_lowercase().contains(&q)
                })
                .cloned()
                .collect();
        }
        if self.visible_articles().is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(0));
        }
    }

    fn clear_search(&mut self) {
        self.search_active = false;
        self.search_query.clear();
        self.filtered_articles.clear();
        if !self.all_articles.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    fn populate(&mut self, mut all: Vec<Article>) {
        all.sort_by(|a, b| b.parsed_date.cmp(&a.parsed_date));

        self.breaking = all.iter().take(self.breaking_count).cloned().collect();
        self.all_articles = all.into_iter().skip(self.breaking_count).collect();

        self.apply_filter();

        self.last_updated = Some(Utc::now());
        self.loading = false;
        self.error = None;
    }

    fn tick_spinner(&mut self) {
        self.spinner_tick = (self.spinner_tick + 1) % SPINNER_FRAMES.len();
    }

    fn tick_ticker(&mut self) {
        self.ticker_offset += 1;
    }

    fn spinner(&self) -> &'static str {
        SPINNER_FRAMES[self.spinner_tick]
    }

    fn show_toast(&mut self, msg: &str) {
        self.toast = Some((msg.to_string(), std::time::Instant::now()));
    }

    fn active_toast(&self) -> Option<&str> {
        if let Some((msg, time)) = &self.toast {
            if time.elapsed() < std::time::Duration::from_secs(2) {
                return Some(msg.as_str());
            }
        }
        None
    }

    fn is_loading_current_article(&self) -> bool {
        if let (Some(loading_link), Some(article)) =
            (&self.loading_article, self.selected_article())
        {
            loading_link == &article.link
        } else {
            false
        }
    }
}

// ── Fetch RSS ─────────────────────────────────────────────────────────────────

async fn fetch_all_feeds(feeds: &[FeedSource]) -> (Vec<Article>, Vec<String>) {
    let futures: Vec<_> = feeds.iter().map(|source| {
        let source = source.clone();
        async move { (source.name.clone(), fetch_feed(&source).await) }
    }).collect();

    let results = futures::future::join_all(futures).await;

    let mut all = Vec::new();
    let mut errors = Vec::new();
    for (name, result) in results {
        match result {
            Ok(articles) => all.extend(articles),
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }

    (all, errors)
}

async fn fetch_feed(source: &FeedSource) -> Result<Vec<Article>, String> {
    let content = reqwest::get(&source.url)
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
                source: source.name.clone(),
                source_color: source.accent,
            }
        })
        .collect();

    Ok(articles)
}

// ── Article content extraction ────────────────────────────────────────────────

async fn fetch_article_content(link: &str) -> Result<String, String> {
    let response = reqwest::get(link)
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    let html = response
        .text()
        .await
        .map_err(|e| format!("Read failed: {e}"))?;

    let url = url::Url::parse(link).map_err(|e| format!("URL parse failed: {e}"))?;

    // Use readability to extract the main content
    let mut cursor = std::io::Cursor::new(html.as_bytes());
    match readability::extractor::extract(&mut cursor, &url) {
        Ok(product) => {
            let text = strip_html(&product.content);
            if text.is_empty() {
                Err("Could not extract article content".into())
            } else {
                Ok(text)
            }
        }
        Err(e) => Err(format!("Extraction failed: {e}")),
    }
}

fn spawn_feed_fetch(feeds: Vec<FeedSource>, tx: &mpsc::UnboundedSender<AppMessage>) {
    let tx = tx.clone();
    tokio::spawn(async move {
        let (articles, errors) = fetch_all_feeds(&feeds).await;
        if articles.is_empty() && !errors.is_empty() {
            let _ = tx.send(AppMessage::FeedError(errors.join("; ")));
        } else {
            if !errors.is_empty() {
                let failed: Vec<&str> = errors.iter().map(|e| e.split(':').next().unwrap_or("?")).collect();
                let _ = tx.send(AppMessage::PartialError(format!("Failed: {}", failed.join(", "))));
            }
            let _ = tx.send(AppMessage::FeedResult(articles));
        }
    });
}

fn spawn_article_fetch(link: String, tx: &mpsc::UnboundedSender<AppMessage>) {
    let tx = tx.clone();
    tokio::spawn(async move {
        match fetch_article_content(&link).await {
            Ok(content) => {
                let _ = tx.send(AppMessage::ArticleContent {
                    link,
                    content,
                });
            }
            Err(error) => {
                let _ = tx.send(AppMessage::ArticleFetchError { link, error });
            }
        }
    });
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
    // Collapse multiple blank lines into at most two newlines
    let mut result = String::with_capacity(output.len());
    let mut blank_count = 0;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blank_count += 1;
            if blank_count <= 1 {
                result.push('\n');
            }
        } else {
            blank_count = 0;
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(trimmed);
            result.push('\n');
        }
    }
    result.trim().to_string()
}

// ── UI rendering ──────────────────────────────────────────────────────────────

fn ui(f: &mut Frame, app: &App) {
    let breaking_height = if app.breaking.is_empty() || app.view == View::Detail {
        0
    } else {
        1
    };

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(breaking_height),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.area());

    render_header(f, app, outer[0]);

    if app.loading && app.all_articles.is_empty() {
        let spinner = app.spinner();
        let loading = Paragraph::new(Line::from(vec![
            Span::styled(format!("  {spinner} "), Style::default().fg(theme::SPINNER)),
            Span::styled("Fetching feeds...", Style::default().fg(theme::FG_DIM)),
        ]));
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
                let wide = outer[2].width >= 100;
                if wide {
                    let panes = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
                        .split(outer[2]);
                    render_list(f, app, panes[0]);
                    render_preview(f, app, panes[1]);
                } else {
                    render_list(f, app, outer[2]);
                }
            }
            View::Detail => {
                let full = Rect {
                    x: outer[1].x,
                    y: outer[1].y,
                    width: outer[1].width,
                    height: outer[1].height + outer[2].height,
                };
                render_detail(f, app, full);
            }
        }
    }

    render_footer(f, app, outer[3]);
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let updated_text = match (app.loading, app.last_updated) {
        (true, Some(_)) => {
            let s = app.spinner();
            format!("{s} Refreshing...")
        }
        (true, None) => String::new(),
        (false, Some(time)) => {
            let ago = Utc::now().signed_duration_since(time);
            if ago.num_seconds() < 60 {
                "Updated just now".to_string()
            } else {
                format!("Updated {}m ago", ago.num_minutes())
            }
        }
        (false, None) => String::new(),
    };

    let right_str = if let Some(toast) = app.active_toast() {
        toast.to_string()
    } else {
        updated_text
    };

    let right_color = if app.active_toast().is_some() {
        theme::SUCCESS
    } else {
        theme::FG_DIM
    };

    let mut title_spans = vec![
        Span::styled(
            " newsterm ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("│ ", Style::default().fg(theme::BORDER)),
    ];

    for (i, feed) in app.feeds.iter().enumerate() {
        if i > 0 {
            title_spans.push(Span::styled(" + ", Style::default().fg(theme::FG_DIM)));
        }
        title_spans.push(Span::styled(
            feed.name.as_str(),
            Style::default()
                .fg(feed.accent)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let title_line = Line::from(title_spans);

    let right_text = Line::from(Span::styled(
        format!("{right_str} "),
        Style::default().fg(right_color),
    ));

    let header_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(right_str.len() as u16 + 2)])
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

    // Build the full ticker string: " ▲ BREAKING │ Title1 (source) ··· Title2 (source) ··· "
    let separator = "  ◆  ";
    let mut ticker_text = String::new();
    for (i, article) in app.breaking.iter().enumerate() {
        if i > 0 {
            ticker_text.push_str(separator);
        }
        ticker_text.push_str(&article.title);
        ticker_text.push_str(&format!(" ({})", article.source));
    }

    let label = " ▲ BREAKING ";
    let label_len = label.len();
    let ticker_area_width = (area.width as usize).saturating_sub(label_len);

    if ticker_area_width == 0 {
        return;
    }

    // Double the text so it loops seamlessly
    let full_ticker = format!("{ticker_text}{separator}{ticker_text}{separator}");
    let ticker_chars: Vec<char> = full_ticker.chars().collect();
    let total_len = ticker_chars.len();

    // Get the visible slice based on offset
    let offset = app.ticker_offset % total_len.max(1);
    let visible: String = ticker_chars
        .iter()
        .cycle()
        .skip(offset)
        .take(ticker_area_width)
        .collect();

    let line = Line::from(vec![
        Span::styled(
            label,
            Style::default()
                .fg(Color::White)
                .bg(theme::BREAKING_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {visible}"),
            Style::default()
                .fg(Color::White)
                .bg(theme::BREAKING_BG),
        ),
    ]);

    f.render_widget(Paragraph::new(line), area);
}

fn render_list(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::NONE)
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let search_height = if app.search_active || !app.search_query.is_empty() {
        2
    } else {
        0
    };

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(search_height),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .split(inner);

    if search_height > 0 {
        let cursor = if app.search_active { "▊" } else { "" };
        let search_line = Line::from(vec![
            Span::styled(" / ", Style::default().fg(theme::SPINNER).add_modifier(Modifier::BOLD)),
            Span::styled(&app.search_query, Style::default().fg(Color::White)),
            Span::styled(cursor, Style::default().fg(theme::SPINNER)),
        ]);
        let search_block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(theme::BORDER));
        let search = Paragraph::new(search_line).block(search_block);
        f.render_widget(search, layout[0]);
    }

    let visible = app.visible_articles();

    let section_title = if !app.search_query.is_empty() {
        Line::from(vec![
            Span::styled(
                " SEARCH RESULTS ",
                Style::default()
                    .fg(theme::SPINNER)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {} matches", visible.len()),
                Style::default().fg(theme::FG_DIM),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                " LATEST NEWS ",
                Style::default()
                    .fg(theme::FG_MUTED)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {} articles", visible.len()),
                Style::default().fg(theme::FG_DIM),
            ),
        ])
    };
    f.render_widget(
        Paragraph::new(section_title).block(Block::default().padding(Padding::new(0, 0, 0, 0))),
        layout[1],
    );

    if visible.is_empty() && !app.search_query.is_empty() {
        let empty_msg = Paragraph::new(Line::from(vec![
            Span::styled("  No articles matching \"", Style::default().fg(theme::FG_DIM)),
            Span::styled(&app.search_query, Style::default().fg(Color::White)),
            Span::styled("\"", Style::default().fg(theme::FG_DIM)),
        ]));
        f.render_widget(empty_msg, layout[2]);
        return;
    }

    let list_width = layout[2].width as usize;
    let items: Vec<ListItem> = visible
        .iter()
        .map(|article| {
            let age = article.age_label();
            let is_read = app.store.is_read(&article.link);
            let is_bookmarked = app.store.is_bookmarked(&article.link);

            let title_style = if is_read {
                Style::default().fg(theme::FG_READ)
            } else {
                Style::default().fg(theme::FG)
            };

            let bookmark_icon = if is_bookmarked { "★ " } else { "" };

            // Calculate max title length: total width minus prefix and suffix
            // prefix: " " + badge + "  " + bookmark_icon ≈ 3 + name.len() + 2 + bookmark
            // suffix: "  " + age
            let prefix_len = 3 + article.source.len() + 2 + bookmark_icon.len();
            let suffix_len = 2 + age.len();
            let max_title = list_width.saturating_sub(prefix_len + suffix_len + 4); // 4 for highlight symbol + padding

            let title = if article.title.chars().count() > max_title && max_title > 3 {
                let truncated: String = article.title.chars().take(max_title.saturating_sub(1)).collect();
                format!("{truncated}…")
            } else {
                article.title.clone()
            };

            let line = Line::from(vec![
                Span::styled(" ", Style::default()),
                source_badge(&article.source, article.source_color),
                Span::styled("  ", Style::default()),
                Span::styled(bookmark_icon, Style::default().fg(theme::BOOKMARK)),
                Span::styled(title, title_style),
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
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" ▌");

    f.render_stateful_widget(list, layout[2], &mut app.list_state.clone());
}

fn render_preview(f: &mut Frame, app: &App, area: Rect) {
    let article = match app.selected_article() {
        Some(a) => a,
        None => {
            let empty = Paragraph::new("  Select an article to preview")
                .style(Style::default().fg(theme::FG_DIM))
                .block(
                    Block::default()
                        .borders(Borders::LEFT)
                        .border_style(Style::default().fg(theme::BORDER))
                        .padding(Padding::new(1, 1, 1, 0)),
                );
            f.render_widget(empty, area);
            return;
        }
    };

    let inner_block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(theme::BORDER))
        .padding(Padding::new(2, 2, 1, 0));
    let inner = inner_block.inner(area);
    f.render_widget(inner_block, area);

    let content_width = inner.width as usize;
    if content_width == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    let bookmark_span = if app.store.is_bookmarked(&article.link) {
        Span::styled(" ★", Style::default().fg(theme::BOOKMARK))
    } else {
        Span::styled("", Style::default())
    };

    lines.push(Line::from(vec![
        source_badge(&article.source, article.source_color),
        Span::styled(
            format!("  {}", article.age_label()),
            Style::default().fg(theme::FG_DIM),
        ),
        bookmark_span,
    ]));
    lines.push(Line::default());

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

    if !article.pub_date.is_empty() {
        lines.push(Line::from(Span::styled(
            &article.pub_date,
            Style::default().fg(theme::FG_DIM),
        )));
        lines.push(Line::default());
    }

    lines.push(Line::from(Span::styled(
        "─".repeat(content_width.min(50)),
        Style::default().fg(theme::BORDER),
    )));
    lines.push(Line::default());

    let wrap_width = content_width.min(70);
    for paragraph in article.description.split('\n') {
        let trimmed = paragraph.trim();
        if trimmed.is_empty() {
            lines.push(Line::default());
        } else {
            let para_wrapped = wrap(trimmed, wrap_width);
            for l in &para_wrapped {
                lines.push(Line::from(Span::styled(
                    l.to_string(),
                    Style::default().fg(theme::FG),
                )));
            }
            lines.push(Line::default());
        }
    }

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);
}

fn render_detail(f: &mut Frame, app: &App, area: Rect) {
    let article = match app.selected_article() {
        Some(a) => a,
        None => return,
    };

    f.render_widget(Clear, area);

    let h_margin = if area.width > 120 { 8 } else if area.width > 80 { 6 } else { 3 };
    let inner = area.inner(Margin {
        vertical: 1,
        horizontal: h_margin,
    });

    let content_width = inner.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();

    let bookmark_span = if app.store.is_bookmarked(&article.link) {
        Span::styled(" ★ bookmarked", Style::default().fg(theme::BOOKMARK))
    } else {
        Span::styled("", Style::default())
    };

    let position = app.list_state.selected().map(|i| {
        format!("  {}/{}", i + 1, app.visible_articles().len())
    }).unwrap_or_default();

    lines.push(Line::from(vec![
        source_badge(&article.source, article.source_color),
        Span::styled(
            format!("  {}", article.age_label()),
            Style::default().fg(theme::FG_DIM),
        ),
        bookmark_span,
        Span::styled(position, Style::default().fg(theme::FG_MUTED)),
    ]));
    lines.push(Line::default());

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

    if !article.pub_date.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {}", article.pub_date),
            Style::default().fg(theme::FG_DIM),
        )));
        lines.push(Line::default());
    }

    lines.push(Line::from(Span::styled(
        "─".repeat(content_width.min(60)),
        Style::default().fg(theme::BORDER),
    )));
    lines.push(Line::default());

    // Show full article content if available, otherwise show loading or RSS description
    let wrap_width = content_width.min(72);
    if let Some(content) = app.content_cache.get(&article.link) {
        if wrap_width > 0 {
            for paragraph in content.split('\n') {
                let trimmed = paragraph.trim();
                if trimmed.is_empty() {
                    lines.push(Line::default());
                } else {
                    let para_wrapped = wrap(trimmed, wrap_width);
                    for l in &para_wrapped {
                        lines.push(Line::from(Span::styled(
                            l.to_string(),
                            Style::default().fg(theme::FG),
                        )));
                    }
                    lines.push(Line::default());
                }
            }
        }
    } else if app.is_loading_current_article() {
        let spinner = app.spinner();
        lines.push(Line::from(vec![
            Span::styled(format!("  {spinner} "), Style::default().fg(theme::SPINNER)),
            Span::styled(
                "Loading full article...",
                Style::default().fg(theme::FG_DIM),
            ),
        ]));
        lines.push(Line::default());
        // Show RSS description as preview while loading
        if wrap_width > 0 && !article.description.is_empty() {
            lines.push(Line::from(Span::styled(
                "Preview:",
                Style::default()
                    .fg(theme::FG_MUTED)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::default());
            let desc_wrapped = wrap(&article.description, wrap_width);
            for l in &desc_wrapped {
                lines.push(Line::from(Span::styled(
                    l.to_string(),
                    Style::default().fg(theme::FG_DIM),
                )));
            }
        }
    } else {
        // Fallback: just show RSS description
        if wrap_width > 0 {
            for paragraph in article.description.split('\n') {
                let trimmed = paragraph.trim();
                if trimmed.is_empty() {
                    lines.push(Line::default());
                } else {
                    let para_wrapped = wrap(trimmed, wrap_width);
                    for l in &para_wrapped {
                        lines.push(Line::from(Span::styled(
                            l.to_string(),
                            Style::default().fg(theme::FG),
                        )));
                    }
                    lines.push(Line::default());
                }
            }
        }
    }

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

    let total_lines = lines.len() as u16;
    let max_scroll = total_lines.saturating_sub(inner.height / 2);
    let scroll = app.scroll_offset.min(max_scroll);

    let paragraph = Paragraph::new(Text::from(lines))
        .scroll((scroll, 0))
        .block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(article.source_color))
                .padding(Padding::new(1, 0, 0, 0)),
        );

    f.render_widget(paragraph, inner);

    // Scroll position indicator
    if total_lines > inner.height {
        let pct = if max_scroll == 0 {
            100
        } else {
            ((scroll as u32 * 100) / max_scroll as u32).min(100)
        };
        let label = if scroll == 0 {
            " TOP ".to_string()
        } else if pct >= 100 {
            " END ".to_string()
        } else {
            format!(" {pct}% ")
        };
        let indicator = Paragraph::new(Span::styled(
            &label,
            Style::default().fg(theme::FG_DIM).bg(theme::FOOTER_BG),
        ))
        .alignment(ratatui::layout::Alignment::Right);
        let indicator_area = Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(1),
            width: inner.width,
            height: 1,
        };
        f.render_widget(indicator, indicator_area);
    }
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let help = match app.view {
        View::List if app.search_active => vec![
            Span::styled(" Type ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("to filter  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("Esc ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("clear  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("Enter ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("done", Style::default().fg(theme::FG_DIM)),
        ],
        View::List => vec![
            Span::styled(" ↑/↓ j/k ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("nav  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("Enter ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("read  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("/ ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("search  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("b ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("bookmark  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("r ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("refresh  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("q ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("quit", Style::default().fg(theme::FG_DIM)),
        ],
        View::Detail => vec![
            Span::styled(" q/Esc ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("back  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("↑/↓ ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("scroll  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("n/N ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("next/prev  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("o ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("open  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("y ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("copy  ", Style::default().fg(theme::FG_DIM)),
            Span::styled("b ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled("bookmark", Style::default().fg(theme::FG_DIM)),
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
    let cfg = Config::load();
    let feeds = FeedSource::from_config(&cfg.feeds);
    let auto_refresh = std::time::Duration::from_secs(cfg.auto_refresh_secs);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(feeds.clone(), cfg.breaking_count);

    let (tx, mut rx) = mpsc::unbounded_channel::<AppMessage>();

    spawn_feed_fetch(feeds.clone(), &tx);

    let mut spinner_interval = tokio::time::interval(std::time::Duration::from_millis(80));
    let mut ticker_interval = tokio::time::interval(std::time::Duration::from_millis(200));
    let mut auto_refresh_interval = tokio::time::interval(auto_refresh);
    auto_refresh_interval.tick().await;

    let mut event_reader = EventStream::new();

    loop {
        terminal.draw(|f| ui(f, &app))?;

        while let Ok(msg) = rx.try_recv() {
            match msg {
                AppMessage::FeedResult(articles) => app.populate(articles),
                AppMessage::FeedError(e) => {
                    app.error = Some(e);
                    app.loading = false;
                }
                AppMessage::PartialError(msg) => {
                    app.show_toast(&msg);
                }
                AppMessage::ArticleContent { link, content } => {
                    if app.loading_article.as_deref() == Some(&link) {
                        app.loading_article = None;
                    }
                    app.content_cache.insert(link, content);
                }
                AppMessage::ArticleFetchError { link, error } => {
                    if app.loading_article.as_deref() == Some(&link) {
                        app.loading_article = None;
                    }
                    app.content_cache
                        .insert(link, format!("[Could not load full article: {error}]"));
                }
            }
        }

        tokio::select! {
            _ = spinner_interval.tick() => {
                if app.loading || app.loading_article.is_some() {
                    app.tick_spinner();
                }
            }
            _ = ticker_interval.tick() => {
                if !app.breaking.is_empty() {
                    app.tick_ticker();
                }
            }
            _ = auto_refresh_interval.tick() => {
                if !app.loading {
                    app.loading = true;
                    app.error = None;
                    spawn_feed_fetch(feeds.clone(), &tx);
                }
            }
            event = event_reader.next() => {
                let Some(Ok(Event::Key(key))) = event else { continue };
                if key.kind != KeyEventKind::Press { continue; }

                let key_code = key.code;
                let modifiers = key.modifiers;
                let term_height = terminal.size().map(|s| s.height).unwrap_or(24);
                let page_size = (term_height / 2) as usize;

                match app.view {
                    View::List if app.search_active => match key_code {
                        KeyCode::Esc => app.clear_search(),
                        KeyCode::Enter => app.search_active = false,
                        KeyCode::Backspace => {
                            app.search_query.pop();
                            app.apply_filter();
                        }
                        KeyCode::Char(c) => {
                            app.search_query.push(c);
                            app.apply_filter();
                        }
                        KeyCode::Down => app.next_article(),
                        KeyCode::Up => app.prev_article(),
                        _ => {}
                    },
                    View::List => match key_code {
                        KeyCode::Char('q') => break,
                        KeyCode::Down | KeyCode::Char('j') => app.next_article(),
                        KeyCode::Up | KeyCode::Char('k') => app.prev_article(),
                        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                            app.page_down(page_size);
                        }
                        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                            app.page_up(page_size);
                        }
                        KeyCode::Char('G') => {
                            let len = app.visible_articles().len();
                            if len > 0 {
                                app.list_state.select(Some(len - 1));
                            }
                        }
                        KeyCode::Char('g') => {
                            if !app.visible_articles().is_empty() {
                                app.list_state.select(Some(0));
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(article) = app.selected_article() {
                                let link = article.link.clone();
                                app.store.mark_read(&link);
                                app.scroll_offset = 0;
                                app.view = View::Detail;

                                if !link.is_empty() && !app.content_cache.contains_key(&link) {
                                    app.loading_article = Some(link.clone());
                                    spawn_article_fetch(link, &tx);
                                }
                            }
                        }
                        KeyCode::Char('/') => {
                            app.search_active = true;
                            app.search_query.clear();
                        }
                        KeyCode::Char('b') => {
                            if let Some(article) = app.selected_article() {
                                let (title, link, source) = (
                                    article.title.clone(),
                                    article.link.clone(),
                                    article.source.clone(),
                                );
                                let added = app.store.toggle_bookmark(&title, &link, &source);
                                if added {
                                    app.show_toast("★ Bookmarked");
                                } else {
                                    app.show_toast("Bookmark removed");
                                }
                            }
                        }
                        KeyCode::Char('r') => {
                            if !app.loading {
                                app.loading = true;
                                app.error = None;
                                spawn_feed_fetch(feeds.clone(), &tx);
                                auto_refresh_interval.reset();
                            }
                        }
                        _ => {}
                    },
                    View::Detail => match key_code {
                        KeyCode::Char('q') => {
                            app.view = View::List;
                        }
                        KeyCode::Esc | KeyCode::Backspace => {
                            app.view = View::List;
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            app.scroll_offset = app.scroll_offset.saturating_add(1);
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.scroll_offset = app.scroll_offset.saturating_sub(1);
                        }
                        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                            app.scroll_offset = app.scroll_offset.saturating_add(page_size as u16);
                        }
                        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                            app.scroll_offset = app.scroll_offset.saturating_sub(page_size as u16);
                        }
                        KeyCode::Char('n') | KeyCode::Right => {
                            app.next_article();
                            app.scroll_offset = 0;
                            if let Some(article) = app.selected_article() {
                                let link = article.link.clone();
                                app.store.mark_read(&link);
                                if !link.is_empty() && !app.content_cache.contains_key(&link) {
                                    app.loading_article = Some(link.clone());
                                    spawn_article_fetch(link, &tx);
                                }
                            }
                        }
                        KeyCode::Char('N') | KeyCode::Left => {
                            app.prev_article();
                            app.scroll_offset = 0;
                            if let Some(article) = app.selected_article() {
                                let link = article.link.clone();
                                app.store.mark_read(&link);
                                if !link.is_empty() && !app.content_cache.contains_key(&link) {
                                    app.loading_article = Some(link.clone());
                                    spawn_article_fetch(link, &tx);
                                }
                            }
                        }
                        KeyCode::Char('o') => {
                            if let Some(article) = app.selected_article() {
                                if !article.link.is_empty() {
                                    let _ = open::that(&article.link);
                                }
                            }
                        }
                        KeyCode::Char('y') => {
                            if let Some(article) = app.selected_article() {
                                if !article.link.is_empty() {
                                    if let Ok(mut ctx) = ClipboardContext::new() {
                                        if ctx.set_contents(article.link.clone()).is_ok() {
                                            app.show_toast("Link copied!");
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Char('b') => {
                            if let Some(article) = app.selected_article() {
                                let (title, link, source) = (
                                    article.title.clone(),
                                    article.link.clone(),
                                    article.source.clone(),
                                );
                                let added = app.store.toggle_bookmark(&title, &link, &source);
                                if added {
                                    app.show_toast("★ Bookmarked");
                                } else {
                                    app.show_toast("Bookmark removed");
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
