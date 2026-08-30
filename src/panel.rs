use std::io::Stdout;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Wrap};

use crate::core::agents::{AgentEntry, State};
use crate::core::board::{EVERYONE, Entry, Group, Kind};
use crate::error::{JumabekError, JumabekResult};

const POLL: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_millis(120);

#[derive(Debug, Default, serde::Deserialize)]
pub struct Running {
    #[serde(default)]
    pub agents: Vec<AgentEntry>,
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub board: Vec<Entry>,
}

impl Running {
    fn group_of(&self, agent: Option<&AgentEntry>) -> Option<&Group> {
        let wanted = agent?.group_id.as_deref()?;
        self.groups.iter().find(|group| group.id == wanted)
    }

    fn board_for(&self, group: Option<&Group>) -> Vec<&Entry> {
        match group {
            Some(group) => self
                .board
                .iter()
                .filter(|entry| entry.group_id == group.id)
                .collect(),
            None => self.board.iter().collect(),
        }
    }
}

struct Panel {
    url: String,
    running: Running,
    chosen: ListState,
    trouble: Option<String>,
}

impl Panel {
    fn new(url: String, first: Running) -> Self {
        let mut chosen = ListState::default();
        if !first.agents.is_empty() {
            chosen.select(Some(0));
        }

        Panel {
            url,
            running: first,
            chosen,
            trouble: None,
        }
    }

    fn selected(&self) -> Option<&AgentEntry> {
        self.running.agents.get(self.chosen.selected()?)
    }

    fn move_by(&mut self, step: isize) {
        let count = self.running.agents.len();
        if count == 0 {
            self.chosen.select(None);
            return;
        }

        let at = self.chosen.selected().unwrap_or(0) as isize;
        let next = (at + step).rem_euclid(count as isize) as usize;
        self.chosen.select(Some(next));
    }

    fn refreshed(&mut self, fresh: Running) {
        let keep = self.selected().map(|entry| entry.agent_id.clone());
        self.running = fresh;
        self.trouble = None;

        let at = keep
            .and_then(|id| {
                self.running
                    .agents
                    .iter()
                    .position(|entry| entry.agent_id == id)
            })
            .or(if self.running.agents.is_empty() {
                None
            } else {
                Some(0)
            });

        self.chosen.select(at);
    }
}

fn colour_of(state: State) -> Color {
    match state {
        State::Running => Color::Green,
        State::AwaitingPermission => Color::Yellow,
        State::Failed => Color::Red,
        State::Finished => Color::DarkGray,
    }
}

fn colour_of_kind(kind: Kind) -> Color {
    match kind {
        Kind::Task => Color::Cyan,
        Kind::Finding => Color::Green,
        Kind::Decision => Color::Magenta,
        Kind::Question => Color::Yellow,
    }
}

fn agent_rows(running: &Running) -> Vec<ListItem<'_>> {
    running
        .agents
        .iter()
        .map(|entry| {
            let indent = "  ".repeat(entry.depth as usize);
            let who = match &entry.role {
                Some(role) => format!("{} {}", entry.short_id(), role),
                None => entry.short_id().to_string(),
            };

            ListItem::new(vec![
                Line::from(vec![
                    Span::raw(indent.clone()),
                    Span::styled(who, Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::styled(
                        entry.state.id().to_string(),
                        Style::default().fg(colour_of(entry.state)),
                    ),
                    Span::styled(
                        format!(
                            "  {}/{}  {}s",
                            entry.iteration, entry.max_iterations, entry.seconds
                        ),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                Line::from(Span::styled(
                    format!("{}  {}", indent, entry.doing),
                    Style::default().fg(Color::Gray),
                )),
            ])
        })
        .collect()
}

fn board_rows(entries: &[&Entry]) -> Vec<ListItem<'static>> {
    entries
        .iter()
        .map(|entry| {
            let aside = entry.addressee != EVERYONE;
            let indent = if aside { "    " } else { "" };

            ListItem::new(vec![
                Line::from(vec![
                    Span::raw(indent),
                    Span::styled(
                        format!("#{} {}", entry.id, entry.kind.as_str()),
                        Style::default()
                            .fg(colour_of_kind(entry.kind))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(
                            "  {} -> {}",
                            cut(&entry.author, 8),
                            cut(&entry.addressee, 12)
                        ),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        format!("  {}", entry.state.as_str()),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                Line::from(Span::raw(format!(
                    "{}{}",
                    indent,
                    flatten(&entry.body, 200)
                ))),
            ])
        })
        .collect()
}

fn cut(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((at, _)) => text[..at].to_string(),
        None => text.to_string(),
    }
}

fn flatten(text: &str, limit: usize) -> String {
    let one = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match one.char_indices().nth(limit) {
        Some((at, _)) => format!("{}…", &one[..at]),
        None => one,
    }
}

fn draw(frame: &mut ratatui::Frame, panel: &mut Panel) {
    let whole = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(3)])
        .split(frame.area());

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(whole[0]);

    draw_agents(frame, panes[0], panel);
    draw_board(frame, panes[1], panel);
    draw_strip(frame, whole[1], panel);
}

fn draw_agents(frame: &mut ratatui::Frame, area: Rect, panel: &mut Panel) {
    let title = match &panel.trouble {
        Some(why) => format!(" agents — {} ", why),
        None => format!(" agents ({}) ", panel.running.agents.len()),
    };

    if panel.running.agents.is_empty() {
        frame.render_widget(
            Paragraph::new("nothing running")
                .style(Style::default().fg(Color::DarkGray))
                .block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
        return;
    }

    let rows = agent_rows(&panel.running);
    let list = List::new(rows)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().bg(Color::Rgb(40, 40, 40)))
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, area, &mut panel.chosen);
}

fn draw_board(frame: &mut ratatui::Frame, area: Rect, panel: &Panel) {
    let group = panel.running.group_of(panel.selected());
    let entries = panel.running.board_for(group);

    let title = match group {
        Some(group) => format!(" board · {} ", group.id),
        None => " board ".to_string(),
    };

    let block = Block::default().borders(Borders::ALL).title(title);

    if entries.is_empty() {
        frame.render_widget(
            Paragraph::new("nothing written yet")
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: true })
                .block(block),
            area,
        );
        return;
    }

    frame.render_widget(List::new(board_rows(&entries)).block(block), area);
}

fn draw_strip(frame: &mut ratatui::Frame, area: Rect, panel: &Panel) {
    let Some(group) = panel.running.group_of(panel.selected()) else {
        frame.render_widget(
            Paragraph::new(format!(
                "no group · {} · up/down to pick an agent · q to leave",
                panel.url
            ))
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };

    let used = group.spent.min(group.budget);
    let share = if group.budget == 0 {
        0.0
    } else {
        used as f64 / group.budget as f64
    };

    frame.render_widget(
        Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(format!(
                " {} · {} ",
                group.id,
                flatten(&group.goal, 60)
            )))
            .gauge_style(Style::default().fg(if share > 0.8 { Color::Red } else { Color::Blue }))
            .ratio(share.clamp(0.0, 1.0))
            .label(format!("{} of {} shared iterations", used, group.budget)),
        area,
    );
}

pub async fn watch(url: String, mut poll: impl AsyncPoll) -> JumabekResult<()> {
    let first = poll.once().await.map_err(JumabekError::ConfigError)?;

    enable_raw_mode().map_err(|e| JumabekError::ConfigError(e.to_string()))?;
    let mut out = std::io::stdout();
    crossterm::execute!(out, EnterAlternateScreen)
        .map_err(|e| JumabekError::ConfigError(e.to_string()))?;

    let outcome = run(
        &mut Terminal::new(CrosstermBackend::new(out)).map_err(io)?,
        url,
        first,
        &mut poll,
    )
    .await;

    disable_raw_mode().ok();
    crossterm::execute!(std::io::stdout(), LeaveAlternateScreen).ok();

    outcome
}

fn io(e: std::io::Error) -> JumabekError {
    JumabekError::ConfigError(e.to_string())
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    url: String,
    first: Running,
    poll: &mut impl AsyncPoll,
) -> JumabekResult<()> {
    let mut panel = Panel::new(url, first);
    let mut last = std::time::Instant::now();

    loop {
        terminal.draw(|frame| draw(frame, &mut panel)).map_err(io)?;

        if event::poll(TICK).map_err(io)?
            && let Event::Key(key) = event::read().map_err(io)?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Down | KeyCode::Char('j') => panel.move_by(1),
                KeyCode::Up | KeyCode::Char('k') => panel.move_by(-1),
                _ => {}
            }
        }

        if last.elapsed() >= POLL {
            last = std::time::Instant::now();
            match poll.once().await {
                Ok(fresh) => panel.refreshed(fresh),
                Err(why) => panel.trouble = Some(flatten(&why, 40)),
            }
        }
    }
}

pub trait AsyncPoll {
    fn once(&mut self) -> impl std::future::Future<Output = Result<Running, String>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn agent(id: &str, group: Option<&str>) -> AgentEntry {
        let mut entry = AgentEntry::new(id, "do a thing")
            .belonging(group.map(str::to_string), None)
            .allowed(10);
        entry.doing = "skill · shell_executor.execute_command".to_string();
        entry
    }

    fn group(id: &str, budget: u32, spent: u32) -> Group {
        Group {
            id: id.to_string(),
            goal: "find the leak".to_string(),
            budget,
            spent,
        }
    }

    fn entry(id: i64, group: &str, to: &str) -> Entry {
        Entry {
            id,
            group_id: group.to_string(),
            author: "aaaaaaaabbbb".to_string(),
            addressee: to.to_string(),
            kind: Kind::Finding,
            body: "the leak is in parse()".to_string(),
            state: crate::core::board::EntryState::Open,
            created_at: Utc::now().to_rfc3339(),
        }
    }

    fn running() -> Running {
        Running {
            agents: vec![agent("a", Some("g1")), agent("b", Some("g2"))],
            groups: vec![group("g1", 40, 10), group("g2", 40, 39)],
            board: vec![
                entry(1, "g1", EVERYONE),
                entry(2, "g2", "researcher"),
                entry(3, "g1", "b"),
            ],
        }
    }

    #[test]
    fn the_board_shown_is_the_board_of_the_agent_you_picked() {
        let mut panel = Panel::new("u".to_string(), running());

        let group = panel.running.group_of(panel.selected()).cloned();
        assert_eq!(group.as_ref().map(|g| g.id.as_str()), Some("g1"));

        let shown = panel.running.board_for(group.as_ref());
        let ids: Vec<i64> = shown.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 3], "another group's entries were on screen");

        panel.move_by(1);
        let group = panel.running.group_of(panel.selected()).cloned();
        assert_eq!(group.as_ref().map(|g| g.id.as_str()), Some("g2"));
        assert_eq!(
            panel
                .running
                .board_for(group.as_ref())
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn moving_past_the_end_comes_back_to_the_start() {
        let mut panel = Panel::new("u".to_string(), running());

        panel.move_by(1);
        panel.move_by(1);
        assert_eq!(panel.chosen.selected(), Some(0));

        panel.move_by(-1);
        assert_eq!(panel.chosen.selected(), Some(1));
    }

    #[test]
    fn an_empty_answer_leaves_nothing_selected_rather_than_pointing_at_a_gap() {
        let mut panel = Panel::new("u".to_string(), running());
        panel.refreshed(Running::default());

        assert_eq!(panel.chosen.selected(), None);
        assert!(panel.selected().is_none());
    }

    #[test]
    fn a_refresh_keeps_you_on_the_agent_you_were_watching() {
        let mut panel = Panel::new("u".to_string(), running());
        panel.move_by(1);
        let watching = panel.selected().unwrap().agent_id.clone();

        let mut fresh = running();
        fresh.agents.insert(0, agent("newcomer", Some("g3")));
        panel.refreshed(fresh);

        assert_eq!(panel.selected().unwrap().agent_id, watching);
    }

    #[test]
    fn an_agent_that_went_away_drops_you_to_the_top_instead_of_off_the_end() {
        let mut panel = Panel::new("u".to_string(), running());
        panel.move_by(1);

        let mut fresh = running();
        fresh.agents.retain(|entry| entry.agent_id != "b");
        panel.refreshed(fresh);

        assert_eq!(panel.selected().map(|e| e.agent_id.as_str()), Some("a"));
    }

    #[test]
    fn a_failed_agent_is_not_the_same_colour_as_a_working_one() {
        let colours = [
            colour_of(State::Running),
            colour_of(State::AwaitingPermission),
            colour_of(State::Failed),
            colour_of(State::Finished),
        ];

        let mut seen = colours.to_vec();
        seen.sort_by_key(|c| format!("{c:?}"));
        seen.dedup();
        assert_eq!(seen.len(), colours.len(), "two states share a colour");
    }

    #[test]
    fn every_kind_of_entry_reads_differently() {
        let colours = [
            colour_of_kind(Kind::Task),
            colour_of_kind(Kind::Finding),
            colour_of_kind(Kind::Decision),
            colour_of_kind(Kind::Question),
        ];

        let mut seen = colours.to_vec();
        seen.sort_by_key(|c| format!("{c:?}"));
        seen.dedup();
        assert_eq!(seen.len(), colours.len(), "two kinds share a colour");
    }

    #[test]
    fn an_entry_aimed_at_one_agent_is_set_in_from_the_group_wide_ones() {
        let wide = entry(1, "g", EVERYONE);
        let aside = entry(2, "g", "researcher");

        let rows = board_rows(&[&wide, &aside]);
        assert_eq!(rows.len(), 2);

        let rendered = format!("{:?}", rows[1]);
        assert!(
            rendered.contains("    "),
            "an entry addressed to one agent was not indented"
        );
    }

    fn rendered(panel: &mut Panel, width: u16, height: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                .expect("test terminal");
        terminal.draw(|frame| draw(frame, panel)).expect("draw");

        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_three_panes_all_carry_something_worth_reading() {
        let mut panel = Panel::new("http://127.0.0.1:20129/agents".to_string(), running());
        let screen = rendered(&mut panel, 120, 24);

        assert!(screen.contains("agents (2)"), "{screen}");
        assert!(
            screen.contains("running"),
            "the state was not shown\n{screen}"
        );
        assert!(
            screen.contains("shell_executor"),
            "what the agent is doing right now was not shown\n{screen}"
        );
        assert!(
            screen.contains("board · g1"),
            "the board pane named no group\n{screen}"
        );
        assert!(screen.contains("the leak is in parse()"), "{screen}");
        assert!(
            screen.contains("10 of 40 shared iterations"),
            "the bottom strip did not show the shared budget\n{screen}"
        );
        assert!(
            screen.contains("find the leak"),
            "the goal was not shown\n{screen}"
        );
    }

    #[test]
    fn moving_down_swaps_the_board_that_is_drawn() {
        let mut panel = Panel::new("u".to_string(), running());
        panel.move_by(1);
        let screen = rendered(&mut panel, 120, 24);

        assert!(screen.contains("board · g2"), "{screen}");
        assert!(
            screen.contains("39 of 40 shared iterations"),
            "the strip did not follow the selection\n{screen}"
        );
    }

    #[test]
    fn nothing_running_draws_a_panel_rather_than_a_crash() {
        let mut panel = Panel::new("u".to_string(), Running::default());
        let screen = rendered(&mut panel, 100, 20);

        assert!(screen.contains("nothing running"), "{screen}");
        assert!(screen.contains("nothing written yet"), "{screen}");
        assert!(screen.contains("no group"), "{screen}");
    }

    #[test]
    fn a_broken_endpoint_is_said_out_loud_rather_than_shown_as_calm() {
        let mut panel = Panel::new("u".to_string(), running());
        panel.trouble = Some("could not reach it".to_string());
        let screen = rendered(&mut panel, 120, 24);

        assert!(
            screen.contains("could not reach it"),
            "a dead endpoint looked like a quiet one\n{screen}"
        );
    }

    #[test]
    fn a_narrow_terminal_still_draws_without_panicking() {
        let mut panel = Panel::new("u".to_string(), running());
        for (w, h) in [(40, 10), (20, 8), (80, 6)] {
            let screen = rendered(&mut panel, w, h);
            assert!(!screen.is_empty(), "{w}x{h} drew nothing");
        }
    }

    #[test]
    fn a_body_that_runs_on_is_cut_and_flattened_to_one_line() {
        let long = flatten(&format!("first\n{}", "x".repeat(400)), 60);
        assert!(!long.contains('\n'));
        assert!(long.ends_with('…'));
    }
}
