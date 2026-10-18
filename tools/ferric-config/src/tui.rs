//! The interactive editor: a menuconfig-style tree of menus and options.

use std::fmt::Write as _;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::model::{self, Resolved, Source, UserConfig};
use crate::schema::{Entry, Kind, Level, Schema};
use crate::value::Value;
use crate::{Ctx, generate};

/// Plain-text description of an option, shared with `ferric-config explain`.
pub fn describe(schema: &Schema, r: &Resolved, i: usize) -> String {
    let o = &schema.options[i];
    let mut s = String::new();
    let _ = writeln!(s, "{} ({})", o.title, o.id);
    let _ = writeln!(
        s,
        "type: {}   value: {}   default: {}",
        o.kind.name(),
        r.values[i].to_toml(),
        o.default.to_toml()
    );
    match &r.sources[i] {
        Source::User => {
            let _ = writeln!(s, "set in ferric.toml");
        }
        Source::Selected(by) => {
            let _ = writeln!(s, "forced on by {by}");
        }
        Source::Inactive => {
            let _ = writeln!(s, "inactive: dependencies unmet");
        }
        Source::Default => {}
    }
    if let Some((_, d)) = &o.depends {
        let _ = writeln!(s, "depends on: {d}");
    }
    if !o.selects.is_empty() {
        let _ = writeln!(s, "selects: {}", o.selects.join(", "));
    }
    for (_, v, when) in &o.defaults {
        let _ = writeln!(s, "default {} when {when}", v.to_toml());
    }
    if o.kind == Kind::Choice {
        let _ = writeln!(s, "choices:");
        for c in &o.choices {
            let _ = writeln!(s, "  {:<12} {}", c.value, c.title);
        }
    }
    if let (Some(lo), Some(hi)) = (o.min, o.max) {
        let _ = writeln!(s, "range: {lo}..={hi}");
    }
    if o.experimental {
        let _ = writeln!(s, "experimental");
    }
    if !o.help.is_empty() {
        let _ = writeln!(s, "\n{}", o.help.trim_end());
    }
    s
}

enum Mode {
    Normal,
    Choice {
        opt: usize,
        state: ListState,
    },
    Input {
        opt: usize,
        buf: String,
        error: Option<String>,
    },
    Search {
        query: String,
        state: ListState,
    },
    Quit,
    Help,
}

struct App<'a> {
    ctx: &'a Ctx,
    user: UserConfig,
    r: Resolved,
    /// Menu path from the root; empty is the top level.
    path: Vec<String>,
    state: ListState,
    saved_selection: Vec<usize>,
    mode: Mode,
    dirty: bool,
    status: String,
    done: bool,
}

impl<'a> App<'a> {
    fn schema(&self) -> &'a Schema {
        &self.ctx.schema
    }

    fn entries(&self) -> Vec<Entry> {
        match self.path.last() {
            None => self.schema().roots(),
            Some(m) => self.schema().entries(m),
        }
    }

    fn selected(&self) -> Option<Entry> {
        self.entries().get(self.state.selected()?).cloned()
    }

    fn refresh(&mut self) {
        self.r = model::resolve(self.schema(), &self.user.values);
    }

    fn set(&mut self, i: usize, v: Value) {
        let id = self.schema().options[i].id.clone();
        match self.user.set(&id, v) {
            Ok(()) => {
                self.dirty = true;
                self.refresh();
            }
            Err(e) => self.status = e,
        }
    }

    fn reset(&mut self, i: usize) {
        let id = self.schema().options[i].id.clone();
        if self.user.values.contains_key(&id) {
            self.user.unset(&id);
            self.dirty = true;
            self.refresh();
            self.status = format!("{id} reset to its default");
        }
    }

    fn save(&mut self) {
        if let Err(e) = self.user.save() {
            self.status = e;
            return;
        }
        self.dirty = false;
        let ctx = self.ctx;
        self.status = match generate::plan(&ctx.schema, &self.r, &ctx.root, &ctx.out) {
            Ok(plan) if self.r.errors() == 0 => {
                match generate::apply(&ctx.root, &ctx.out, &plan, ctx.touch_recipes) {
                    Ok(a) if a.stale.iter().any(|s| s == "*") => format!(
                        "Saved {}. Every package will rebuild on the next make.",
                        ctx.config_path.display()
                    ),
                    Ok(a) if !a.stale.is_empty() => format!(
                        "Saved {}. Will rebuild: {}.",
                        ctx.config_path.display(),
                        a.stale.join(", ")
                    ),
                    Ok(_) => format!("Saved {}.", ctx.config_path.display()),
                    Err(e) => format!("Saved, but generating failed: {}", crate::error_chain(&e)),
                }
            }
            Ok(_) => format!(
                "Saved {}, but it has errors; make will refuse it.",
                ctx.config_path.display()
            ),
            Err(errs) => format!("Saved, but: {}", errs[0].message),
        };
    }

    fn enter(&mut self, e: Entry) {
        match e {
            Entry::Menu(m) => {
                self.saved_selection
                    .push(self.state.selected().unwrap_or(0));
                self.path.push(m);
                self.state.select(Some(0));
            }
            Entry::Option(i) => self.activate(i),
        }
    }

    fn back(&mut self) {
        if self.path.pop().is_some() {
            self.state
                .select(Some(self.saved_selection.pop().unwrap_or(0)));
        } else {
            self.quit();
        }
    }

    fn quit(&mut self) {
        if self.dirty {
            self.mode = Mode::Quit;
        } else {
            self.done = true;
        }
    }

    fn activate(&mut self, i: usize) {
        if !self.r.active[i] {
            self.status = match &self.schema().options[i].depends {
                Some((_, d)) => format!("Unavailable: needs {d}"),
                None => "Unavailable: its menu's dependencies are unmet".into(),
            };
            return;
        }
        let o = &self.schema().options[i];
        match o.kind {
            Kind::Bool => self.toggle(i),
            Kind::Choice => {
                let cur = self.r.values[i].render();
                let pos = o.choices.iter().position(|c| c.value == cur).unwrap_or(0);
                let mut state = ListState::default();
                state.select(Some(pos));
                self.mode = Mode::Choice { opt: i, state };
            }
            Kind::Int | Kind::String => {
                self.mode = Mode::Input {
                    opt: i,
                    buf: self.r.values[i].render(),
                    error: None,
                }
            }
            Kind::List => {
                let buf = match &self.r.values[i] {
                    Value::List(l) => l.join(", "),
                    v => v.render(),
                };
                self.mode = Mode::Input {
                    opt: i,
                    buf,
                    error: None,
                }
            }
        }
    }

    fn toggle(&mut self, i: usize) {
        if let Source::Selected(by) = &self.r.sources[i] {
            self.status = format!("Forced on by {by}");
            return;
        }
        let cur = self.r.values[i].truthy();
        self.set(i, Value::Bool(!cur));
    }

    fn move_by(&mut self, delta: isize) {
        let n = self.entries().len();
        if n == 0 {
            return;
        }
        let cur = self.state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, n as isize - 1);
        self.state.select(Some(next as usize));
    }

    fn search_results(&self, query: &str) -> Vec<usize> {
        let q = query.to_lowercase();
        self.schema()
            .options
            .iter()
            .enumerate()
            .filter(|(_, o)| {
                q.is_empty()
                    || o.id.to_lowercase().contains(&q)
                    || o.title.to_lowercase().contains(&q)
                    || o.help.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Opens the menu holding option `i` and selects it.
    fn jump_to(&mut self, i: usize) {
        let schema = self.schema();
        let mut chain = vec![schema.options[i].menu.clone()];
        while let Some(p) = schema
            .menu(chain.last().unwrap())
            .and_then(|m| m.parent.clone())
        {
            chain.push(p);
        }
        chain.reverse();
        self.path.clear();
        self.saved_selection.clear();
        let mut entries = schema.roots();
        for m in &chain {
            let pos = entries
                .iter()
                .position(|e| e == &Entry::Menu(m.clone()))
                .unwrap_or(0);
            self.saved_selection.push(pos);
            self.path.push(m.clone());
            entries = schema.entries(m);
        }
        let pos = entries
            .iter()
            .position(|e| e == &Entry::Option(i))
            .unwrap_or(0);
        self.state.select(Some(pos));
    }

    fn key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.done = true;
            return;
        }
        match &mut self.mode {
            Mode::Normal => {
                self.status.clear();
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
                    KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
                    KeyCode::PageUp => self.move_by(-10),
                    KeyCode::PageDown => self.move_by(10),
                    KeyCode::Home | KeyCode::Char('g') => self.state.select(Some(0)),
                    KeyCode::End | KeyCode::Char('G') => self.move_by(isize::MAX / 2),
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                        if let Some(e) = self.selected() {
                            self.enter(e);
                        }
                    }
                    KeyCode::Char(' ') => match self.selected() {
                        Some(Entry::Option(i)) if self.schema().options[i].kind == Kind::Bool => {
                            if self.r.active[i] {
                                self.toggle(i)
                            } else {
                                self.activate(i)
                            }
                        }
                        Some(e) => self.enter(e),
                        None => {}
                    },
                    KeyCode::Char('y') => {
                        if let Some(Entry::Option(i)) = self.selected()
                            && self.schema().options[i].kind == Kind::Bool
                            && self.r.active[i]
                        {
                            self.set(i, Value::Bool(true));
                        }
                    }
                    KeyCode::Char('n') => {
                        if let Some(Entry::Option(i)) = self.selected()
                            && self.schema().options[i].kind == Kind::Bool
                            && self.r.active[i]
                        {
                            if let Source::Selected(by) = &self.r.sources[i] {
                                self.status = format!("Forced on by {by}");
                            } else {
                                self.set(i, Value::Bool(false));
                            }
                        }
                    }
                    KeyCode::Char('d') => {
                        if let Some(Entry::Option(i)) = self.selected() {
                            self.reset(i);
                        }
                    }
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                        self.back()
                    }
                    KeyCode::Char('s') => self.save(),
                    KeyCode::Char('q') => self.quit(),
                    KeyCode::Char('?') => self.mode = Mode::Help,
                    KeyCode::Char('/') => {
                        let mut state = ListState::default();
                        state.select(Some(0));
                        self.mode = Mode::Search {
                            query: String::new(),
                            state,
                        };
                    }
                    _ => {}
                }
            }
            Mode::Choice { opt, state } => {
                let opt = *opt;
                let n = self.ctx.schema.options[opt].choices.len();
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        state.select(Some(state.selected().unwrap_or(0).saturating_sub(1)))
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        state.select(Some((state.selected().unwrap_or(0) + 1).min(n - 1)))
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        let pick = state.selected().unwrap_or(0);
                        let v =
                            Value::Str(self.ctx.schema.options[opt].choices[pick].value.clone());
                        self.mode = Mode::Normal;
                        self.set(opt, v);
                    }
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') => self.mode = Mode::Normal,
                    _ => {}
                }
            }
            Mode::Input { opt, buf, error } => match k.code {
                KeyCode::Enter => {
                    let opt = *opt;
                    match self.ctx.schema.options[opt].parse_cli(buf.trim()) {
                        Ok(v) => {
                            self.mode = Mode::Normal;
                            self.set(opt, v);
                        }
                        Err(e) => *error = Some(e),
                    }
                }
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            },
            Mode::Search { query, state } => match k.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Up => state.select(Some(state.selected().unwrap_or(0).saturating_sub(1))),
                KeyCode::Down => state.select(Some(state.selected().unwrap_or(0) + 1)),
                KeyCode::Backspace => {
                    query.pop();
                    state.select(Some(0));
                }
                KeyCode::Char(c) => {
                    query.push(c);
                    state.select(Some(0));
                }
                KeyCode::Enter => {
                    let q = query.clone();
                    let sel = state.selected().unwrap_or(0);
                    let results = self.search_results(&q);
                    self.mode = Mode::Normal;
                    if let Some(&i) = results.get(sel.min(results.len().saturating_sub(1))) {
                        self.jump_to(i);
                    }
                }
                _ => {}
            },
            Mode::Quit => match k.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    self.save();
                    if !self.dirty {
                        self.done = true;
                    }
                }
                KeyCode::Char('n') => self.done = true,
                _ => self.mode = Mode::Normal,
            },
            Mode::Help => self.mode = Mode::Normal,
        }
    }

    fn row(&self, e: &Entry) -> ListItem<'static> {
        let schema = self.schema();
        let active = self.r.entry_active(schema, e);
        let dim = Style::default().fg(Color::DarkGray);
        let line = match e {
            Entry::Menu(m) => {
                let title = schema.menu(m).map(|m| m.title.clone()).unwrap_or_default();
                Line::from(vec![
                    Span::raw("    "),
                    Span::raw(title),
                    Span::raw("  --->"),
                ])
            }
            Entry::Option(i) => {
                let o = &schema.options[*i];
                let v = &self.r.values[*i];
                let src = &self.r.sources[*i];
                let mark = match (o.kind, v, src) {
                    (Kind::Bool, _, Source::Selected(_)) => "-*-".to_owned(),
                    (Kind::Bool, Value::Bool(true), _) if active => "[*]".to_owned(),
                    (Kind::Bool, _, _) => "[ ]".to_owned(),
                    (Kind::List, Value::List(l), _) => format!("[{}]", l.join(",")),
                    (Kind::String, Value::Str(s), _) => format!("({s:?})"),
                    _ => format!("({})", v.render()),
                };
                let mut spans = vec![Span::raw(format!("{mark} ")), Span::raw(o.title.clone())];
                if *src == Source::User {
                    spans.push(Span::styled(" •", Style::default().fg(Color::Yellow)));
                }
                if o.experimental {
                    spans.push(Span::styled(
                        " (EXPERIMENTAL)",
                        Style::default().fg(Color::Magenta),
                    ));
                }
                Line::from(spans)
            }
        };
        let item = ListItem::new(line);
        if active { item } else { item.style(dim) }
    }

    fn draw(&mut self, f: &mut Frame) {
        let schema = self.schema();
        let [header, body, help, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(9),
            Constraint::Length(2),
        ])
        .areas(f.area());

        let crumbs: Vec<String> = self
            .path
            .iter()
            .filter_map(|m| schema.menu(m).map(|m| m.title.clone()))
            .collect();
        let title = format!(
            " Ferric Oxide configuration{}{}{}",
            if crumbs.is_empty() { "" } else { " > " },
            crumbs.join(" > "),
            if self.dirty { "  [modified]" } else { "" }
        );
        f.render_widget(
            Paragraph::new(title).style(Style::default().add_modifier(Modifier::BOLD).reversed()),
            header,
        );

        let items: Vec<ListItem> = self.entries().iter().map(|e| self.row(e)).collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", self.ctx.config_path.display())),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");
        f.render_stateful_widget(list, body, &mut self.state);

        let help_text = match self.selected() {
            Some(Entry::Option(i)) => describe(schema, &self.r, i),
            Some(Entry::Menu(m)) => schema
                .menu(&m)
                .map(|m| format!("{}\n\n{}", m.title, m.help))
                .unwrap_or_default(),
            None => String::new(),
        };
        f.render_widget(
            Paragraph::new(help_text)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(" Help ")),
            help,
        );

        let errors = self
            .r
            .diags
            .iter()
            .filter(|d| d.level == Level::Error)
            .count();
        let warns = self.r.diags.len() - errors;
        let diag_line = match self.r.diags.first() {
            _ if !self.status.is_empty() => {
                Line::from(self.status.clone()).style(Style::default().fg(Color::Cyan))
            }
            Some(d) => {
                let color = if d.level == Level::Error {
                    Color::Red
                } else {
                    Color::Yellow
                };
                Line::from(format!(
                    "{errors} error(s), {warns} warning(s): {}",
                    d.message
                ))
                .style(Style::default().fg(color))
            }
            None => Line::from(""),
        };
        let keys = Line::from(
            "↑↓ move  ⏎ select  space toggle  y/n on/off  d default  / search  ? help  s save  esc back  q quit",
        )
        .style(Style::default().fg(Color::DarkGray));
        f.render_widget(Paragraph::new(vec![diag_line, keys]), footer);

        match &mut self.mode {
            Mode::Normal => {}
            Mode::Choice { opt, state } => {
                let o = &schema.options[*opt];
                let items: Vec<ListItem> = o
                    .choices
                    .iter()
                    .map(|c| {
                        let cur = self.r.values[*opt].render() == c.value;
                        ListItem::new(format!("({}) {}", if cur { "X" } else { " " }, c.title))
                    })
                    .collect();
                let area = popup(f.area(), 60, items.len() as u16 + 2);
                f.render_widget(Clear, area);
                f.render_stateful_widget(
                    List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title(format!(" {} ", o.title)),
                        )
                        .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
                    area,
                    state,
                );
            }
            Mode::Input { opt, buf, error } => {
                let o = &schema.options[*opt];
                let hint = match o.kind {
                    Kind::List => "comma-separated",
                    Kind::Int => "integer",
                    _ => "text",
                };
                let area = popup(f.area(), 70, 5);
                f.render_widget(Clear, area);
                let mut lines = vec![Line::from(format!("{buf}▏"))];
                lines.push(match error {
                    Some(e) => Line::from(e.clone()).style(Style::default().fg(Color::Red)),
                    None => Line::from(format!("{hint}; ⏎ accept, esc cancel"))
                        .style(Style::default().fg(Color::DarkGray)),
                });
                f.render_widget(
                    Paragraph::new(lines).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(format!(" {} ", o.title)),
                    ),
                    area,
                );
            }
            Mode::Search { query, state } => {
                let results = {
                    let q = query.to_lowercase();
                    schema
                        .options
                        .iter()
                        .filter(|o| {
                            q.is_empty()
                                || o.id.to_lowercase().contains(&q)
                                || o.title.to_lowercase().contains(&q)
                                || o.help.to_lowercase().contains(&q)
                        })
                        .map(|o| ListItem::new(format!("{:<28} {}", o.id, o.title)))
                        .collect::<Vec<_>>()
                };
                if let Some(s) = state.selected()
                    && s >= results.len()
                {
                    state.select(Some(results.len().saturating_sub(1)));
                }
                let area = popup(f.area(), 80, 20);
                f.render_widget(Clear, area);
                f.render_stateful_widget(
                    List::new(results)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title(format!(" Search: {query}▏ ")),
                        )
                        .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
                    area,
                    state,
                );
            }
            Mode::Quit => {
                let area = popup(f.area(), 50, 3);
                f.render_widget(Clear, area);
                f.render_widget(
                    Paragraph::new("Save changes? (y)es  (n)o  (esc) keep editing").block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" Unsaved changes "),
                    ),
                    area,
                );
            }
            Mode::Help => {
                let area = popup(f.area(), 76, 20);
                f.render_widget(Clear, area);
                f.render_widget(
                    Paragraph::new(HELP).wrap(Wrap { trim: false }).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" Help (any key closes) "),
                    ),
                    area,
                );
            }
        }
    }
}

const HELP: &str = "\
Arrow keys or j/k move; Enter or l opens a menu or edits the option;
Esc, h or Backspace goes back. Space toggles a bool, y and n set it.
d resets the option to its default (removes it from ferric.toml).
/ searches ids, titles and help. s saves; q quits.

[*] on   [ ] off   -*- forced on by another option   (x) current value
• marks options set in ferric.toml. Grey entries have unmet dependencies
and do nothing until those are met.

Saving writes ferric.toml (only options you changed) and regenerates
build/ferric/. Recipes whose options changed are marked for rebuild.
Agents can edit ferric.toml directly or use `ferric-config set`.";

fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(4));
    let h = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn event_loop(term: &mut DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    while !app.done {
        term.draw(|f| app.draw(f))?;
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
        {
            app.key(k);
        }
    }
    Ok(())
}

pub fn run(ctx: &Ctx, user: UserConfig) -> Result<(), String> {
    let r = model::resolve(&ctx.schema, &user.values);
    let mut state = ListState::default();
    state.select(Some(0));
    let mut app = App {
        ctx,
        user,
        r,
        path: Vec::new(),
        state,
        saved_selection: Vec::new(),
        mode: Mode::Normal,
        dirty: false,
        status: String::new(),
        done: false,
    };
    let mut term = ratatui::init();
    let res = event_loop(&mut term, &mut app);
    ratatui::restore();
    res.map_err(|e| e.to_string())?;
    if !app.status.is_empty() {
        eprintln!("{}", app.status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyEventState;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent {
            code: c,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn screen(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn navigate_toggle_and_save() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let dir = std::env::temp_dir().join(format!("ferric-config-tui-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = Ctx {
            schema: Schema::load(&root.join("features.toml")).unwrap(),
            root: root.clone(),
            config_path: dir.join("ferric.toml"),
            out: dir.join("out"),
            touch_recipes: false,
        };
        let (user, _) = UserConfig::load(&ctx.config_path, &ctx.schema).unwrap();
        let r = model::resolve(&ctx.schema, &user.values);
        let mut state = ListState::default();
        state.select(Some(0));
        let mut app = App {
            ctx: &ctx,
            user,
            r,
            path: Vec::new(),
            state,
            saved_selection: Vec::new(),
            mode: Mode::Normal,
            dirty: false,
            status: String::new(),
            done: false,
        };
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert!(screen(&term).contains("Processor baseline  --->"));

        // Kernel is the third top-level menu.
        app.key(key(KeyCode::Down));
        app.key(key(KeyCode::Down));
        app.key(key(KeyCode::Enter));
        term.draw(|f| app.draw(f)).unwrap();
        let s = screen(&term);
        assert!(s.contains("-*- ACPI support"), "{s}");
        assert!(s.contains("[ ] NUMA awareness"), "{s}");

        // Multi-core forces ACPI on.
        app.key(key(KeyCode::Char(' ')));
        term.draw(|f| app.draw(f)).unwrap();
        assert!(
            app.status.contains("Forced on by kernel.multi_core"),
            "{}",
            app.status
        );
        app.key(key(KeyCode::Down));
        app.key(key(KeyCode::Char(' ')));
        term.draw(|f| app.draw(f)).unwrap();
        let s = screen(&term);
        assert!(s.contains("[ ] Multi-core (SMP)"), "{s}");
        assert!(s.contains("[*] ACPI support"), "{s}");

        // Search jumps to an option and the choice popup sets it.
        app.key(key(KeyCode::Char('/')));
        for c in "x86_64_level".chars() {
            app.key(key(KeyCode::Char(c)));
        }
        app.key(key(KeyCode::Enter));
        app.key(key(KeyCode::Enter));
        app.key(key(KeyCode::Down));
        app.key(key(KeyCode::Enter));
        assert_eq!(
            app.user.values.get("cpu.x86_64_level"),
            Some(&Value::Str("v4".into()))
        );

        app.key(key(KeyCode::Char('q')));
        assert!(matches!(app.mode, Mode::Quit));
        app.key(key(KeyCode::Char('y')));
        assert!(app.done);
        let saved = std::fs::read_to_string(&ctx.config_path).unwrap();
        assert!(saved.contains("multi_core = false"), "{saved}");
        assert!(saved.contains("x86_64_level = \"v4\""), "{saved}");
        assert!(dir.join("out/config.mk").is_file());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
