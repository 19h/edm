//! The screen's state and what each event does to it \[C53\].
//!
//! Pure: nothing here reads a file, sends a request or touches the terminal.
//! Effects come back as data for the loop in `tui/mod.rs` to carry out, which
//! is what makes every transition a unit test.

use std::collections::VecDeque;

use edm_core::ardent::{ArdentStation, StationMatch};
use edm_core::domain::commander::CommanderState;
use edm_core::render::{Block, write_blocks};
use edm_route::pin::PinKey;

use crate::cmd::route::quick::{QuickSearch, RoundOutcome};
use crate::cmd::route::SurveySearch;
use crate::cmd::sell::{SellRound, SellSearch};
use crate::out::Stream;
use crate::route::follow::FollowState;

use super::autocomplete::{self, Candidate, Kind};
use super::engine::cards::RouteCard;
use super::engine::pins::{PinJob, PinState};
use super::engine::{AuxSpec, Event, JobSpec};
use super::keys::Action;
use super::persist::LastKnown;

/// The screens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Screen {
    Search,
    Results,
    Detail,
    Pins,
    Sell,
    Log,
}

impl Screen {
    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::Search => "Search",
            Self::Results => "Results",
            Self::Detail => "Detail",
            Self::Pins => "Pins",
            Self::Sell => "Sell",
            Self::Log => "Log",
        }
    }
}

/// What kind of search the form describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Quick,
    Survey,
    Sell,
}

impl Mode {
    pub(crate) const ALL: [Self; 3] = [Self::Quick, Self::Survey, Self::Sell];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Quick => "quick lookup",
            Self::Survey => "full survey",
            Self::Sell => "sell the hold",
        }
    }
}

/// How a field is edited and spelled on the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FieldKind {
    /// Free text; empty means "not given".
    Text,
    /// A number; empty means "the command's default".
    Number,
    /// A switch flag, present or absent.
    Switch,
    /// One of a fixed set; the first entry is "not given".
    Choice(&'static [&'static str]),
}

/// What a text field completes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Completes {
    Nothing,
    /// Systems and stations.
    Places,
    /// Commodities, comma separated.
    Commodities,
    Categories,
}

/// One row of the form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Field {
    /// The flag it spells, e.g. `--radius`; `""` for the positional reference.
    pub flag: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    pub kind: FieldKind,
    pub completes: Completes,
    pub modes: &'static [Mode],
    pub text: String,
    pub on: bool,
    pub choice: usize,
}

impl Field {
    const fn new(
        flag: &'static str,
        label: &'static str,
        hint: &'static str,
        kind: FieldKind,
        modes: &'static [Mode],
    ) -> Self {
        Self {
            flag,
            label,
            hint,
            kind,
            completes: Completes::Nothing,
            modes,
            text: String::new(),
            on: false,
            choice: 0,
        }
    }

    const fn completing(mut self, completes: Completes) -> Self {
        self.completes = completes;
        self
    }

    pub(crate) fn applies(&self, mode: Mode) -> bool {
        self.modes.contains(&mode)
    }

    /// What the row shows for its value.
    pub(crate) fn display(&self) -> String {
        match self.kind {
            FieldKind::Text | FieldKind::Number => self.text.clone(),
            FieldKind::Switch => (if self.on { "[x]" } else { "[ ]" }).to_owned(),
            FieldKind::Choice(options) => options[self.choice].to_owned(),
        }
    }

    pub(crate) fn is_text(&self) -> bool {
        matches!(self.kind, FieldKind::Text | FieldKind::Number)
    }
}

const ALL: &[Mode] = &[Mode::Quick, Mode::Survey, Mode::Sell];
const ROUTE: &[Mode] = &[Mode::Quick, Mode::Survey];
const QUICK: &[Mode] = &[Mode::Quick];
const SELL: &[Mode] = &[Mode::Sell];

/// The search form: a list of fields and which one has the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SearchForm {
    pub mode: Mode,
    pub fields: Vec<Field>,
    /// Index into `fields`, always a field that applies to `mode`.
    pub focus: usize,
    /// The last validation or run message, shown under the form.
    pub status: Option<String>,
}

impl Default for SearchForm {
    fn default() -> Self {
        let fields = vec![
            Field::new("", "System or station", "where to search from; a station name is resolved to its system", FieldKind::Text, ROUTE).completing(Completes::Places),
            Field::new("--from", "System", "where the ship is, if the journal does not say", FieldKind::Text, SELL).completing(Completes::Places),
            Field::new("--item", "Items", "commodities, comma separated", FieldKind::Text, ALL).completing(Completes::Commodities),
            Field::new("--category", "Category", "a commodity category, instead of items", FieldKind::Text, QUICK).completing(Completes::Categories),
            Field::new("--quick", "Hops per commodity", "best sellers and buyers kept per commodity, default 5", FieldKind::Number, QUICK),
            Field::new("--radius", "Radius (Ly)", "default 30 for a survey, 500 for a lookup", FieldKind::Number, ALL),
            Field::new("--cargo", "Cargo (t)", "hold size; from the journal when blank", FieldKind::Number, ROUTE),
            Field::new("--credits", "Credits", "buying power; from the journal when blank", FieldKind::Number, ROUTE),
            Field::new("--jump", "Jump range (Ly)", "from the journal when blank", FieldKind::Number, ROUTE),
            Field::new("--shape", "Shape", "", FieldKind::Choice(&["", "one-way", "round-trip", "loop"]), ROUTE),
            Field::new("--top", "Routes shown", "default 20", FieldKind::Number, ALL),
            Field::new("--qty", "Minimum quantity (t)", "seller stock and published buyer demand floor", FieldKind::Number, ROUTE),
            Field::new("--worth", "Your time (cr/h)", "a further stop is taken only when it beats this", FieldKind::Number, SELL),
            Field::new("--stops", "Stops", "markets to spread the sale across, 1 to 4", FieldKind::Number, SELL),
            Field::new("--min-demand", "Minimum demand (t)", "ignore buyers publishing less", FieldKind::Number, SELL),
            Field::new("--pad", "Landing pad", "", FieldKind::Choice(&["", "S", "M", "L"]), ALL),
            Field::new("--carriers", "Fleet carriers", "include carriers; their docking access is checked", FieldKind::Switch, ALL),
            Field::new("--carrier-access", "Carrier access", "", FieldKind::Choice(&["", "open", "any"]), ALL),
            Field::new("--from-here", "From here", "every route departs from where the ship is docked", FieldKind::Switch, QUICK),
            Field::new("--by-profit", "Rank by profit", "credits per run rather than per hour", FieldKind::Switch, ROUTE),
            Field::new("--per-hour", "Show cr/h", "", FieldKind::Switch, ROUTE),
            Field::new("--include-illegal", "Include illegal", "", FieldKind::Switch, ALL),
            Field::new("--max-requests", "Request ceiling", "nothing is sent above it", FieldKind::Number, ALL),
            Field::new("--rps", "Requests per second", "default 4", FieldKind::Number, ALL),
            Field::new("--max-age", "Cache age (min)", "rank from cached prices younger than this", FieldKind::Number, ALL),
            Field::new("--no-cache", "Ignore the cache", "", FieldKind::Switch, ALL),
            Field::new("--yes", "Skip confirmation", "proceed above 250 requests without asking", FieldKind::Switch, ALL),
        ];
        Self {
            mode: Mode::Quick,
            fields,
            focus: 0,
            status: None,
        }
    }
}

impl SearchForm {
    /// The rows that apply to the current mode, in order, with their index.
    pub(crate) fn visible(&self) -> Vec<usize> {
        self.fields
            .iter()
            .enumerate()
            .filter(|(_, field)| field.applies(self.mode))
            .map(|(index, _)| index)
            .collect()
    }

    pub(crate) fn focused(&self) -> &Field {
        &self.fields[self.focus]
    }

    fn focused_mut(&mut self) -> &mut Field {
        &mut self.fields[self.focus]
    }

    fn step(&mut self, delta: isize) {
        let visible = self.visible();
        let Some(at) = visible.iter().position(|index| *index == self.focus) else {
            self.focus = visible.first().copied().unwrap_or(0);
            return;
        };
        let next = (at as isize + delta).rem_euclid(visible.len() as isize) as usize;
        self.focus = visible[next];
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        if !self.fields[self.focus].applies(mode) {
            self.focus = self.visible().first().copied().unwrap_or(0);
        }
    }

    fn cycle_mode(&mut self, delta: isize) {
        let at = Mode::ALL.iter().position(|m| *m == self.mode).unwrap_or(0);
        let next = (at as isize + delta).rem_euclid(Mode::ALL.len() as isize) as usize;
        self.set_mode(Mode::ALL[next]);
    }

    /// The command line this form describes.
    pub(crate) fn argv(&self) -> Vec<String> {
        let mut argv = vec![match self.mode {
            Mode::Quick | Mode::Survey => "route".to_owned(),
            Mode::Sell => "sell".to_owned(),
        }];
        let trimmed = |text: &str| edm_core::js::text::js_trim(text).to_owned();
        for field in self.fields.iter().filter(|field| field.applies(self.mode)) {
            match field.kind {
                FieldKind::Text | FieldKind::Number => {
                    let value = trimmed(&field.text);
                    if value.is_empty() {
                        if field.flag == "--quick" {
                            argv.push("--quick".to_owned());
                            argv.push("5".to_owned());
                        }
                        continue;
                    }
                    if field.flag.is_empty() {
                        argv.push(value);
                    } else {
                        argv.push(field.flag.to_owned());
                        argv.push(value);
                    }
                }
                FieldKind::Switch => {
                    if field.on {
                        argv.push(field.flag.to_owned());
                    }
                }
                FieldKind::Choice(options) => {
                    if field.choice > 0 {
                        argv.push(field.flag.to_owned());
                        argv.push(options[field.choice].to_owned());
                    }
                }
            }
        }
        argv
    }

    /// Fill the form from an argv it produced earlier.
    pub(crate) fn load(&mut self, argv: &[String]) {
        let Some(command) = argv.first() else { return };
        let mut mode = if command == "sell" { Mode::Sell } else { Mode::Survey };
        if command == "route" && argv.iter().any(|token| token == "--quick") {
            mode = Mode::Quick;
        }
        for field in &mut self.fields {
            field.text.clear();
            field.on = false;
            field.choice = 0;
        }
        self.set_mode(mode);
        let mut tokens = argv.iter().skip(1).peekable();
        while let Some(token) = tokens.next() {
            if !token.starts_with("--") {
                if let Some(field) = self.fields.iter_mut().find(|f| f.flag.is_empty()) {
                    field.text.clone_from(token);
                }
                continue;
            }
            let Some(field) = self.fields.iter_mut().find(|f| f.flag == token) else {
                tokens.next();
                continue;
            };
            match field.kind {
                FieldKind::Switch => field.on = true,
                FieldKind::Choice(options) => {
                    if let Some(value) = tokens.next() {
                        field.choice = options.iter().position(|o| o == value).unwrap_or(0);
                    }
                }
                FieldKind::Text | FieldKind::Number => {
                    if let Some(value) = tokens.next() {
                        field.text.clone_from(value);
                    }
                }
            }
        }
    }
}

/// One line of the log pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LogLine {
    pub stream: Stream,
    pub text: String,
}

/// What the loop must do after a reduction.
pub(crate) enum Effect {
    Quit,
    /// Start the network job; the loop refuses it if one is running.
    StartJob(JobSpec),
    /// Start an aside, which may run beside a job.
    StartAux(AuxSpec),
    /// Answer the pending gate.
    AnswerGate(bool),
    /// Drop the running search.
    CancelJob,
    SavePins,
    SaveSearch(Vec<String>),
    Copy(String),
    ReadJournal,
}

/// A window over everything else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Modal {
    Help,
    Message(String),
    /// A sweep waits for consent: the plan, and the instruction.
    Confirm { lines: Vec<String>, message: String },
    /// Something was sent to the clipboard; show it too, in case it was not
    /// taken.
    Copied(String),
}

/// Whether a route's prices were measured this round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LiveStatus {
    Live,
    Cached,
    /// Dropped by the last round: a side lost stock or demand.
    Unpriced,
    /// A round is reading it now.
    Verifying,
}

impl LiveStatus {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Cached => "cache",
            Self::Unpriced => "unpriced",
            Self::Verifying => "reading…",
        }
    }
}

/// What the results table is ordered by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sort {
    Rank,
    Profit,
    Distance,
    Approach,
    Time,
}

impl Sort {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Rank => "rank",
            Self::Profit => "profit",
            Self::Distance => "distance",
            Self::Approach => "approach",
            Self::Time => "time",
        }
    }
}

/// One route on the results screen.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RouteRow {
    pub rank: usize,
    pub card: RouteCard,
    pub status: LiveStatus,
}

/// The last search's answer.
pub(crate) struct Results {
    pub argv: Vec<String>,
    pub quick: bool,
    /// The solved instance, away while a round re-prices it. A survey's is
    /// not kept: its shortlist is not re-read as a whole, only its pins are.
    pub data: Option<Box<QuickSearch>>,
    pub rows: Vec<RouteRow>,
    pub stations: Vec<ArdentStation>,
    pub selected: usize,
    pub sort: Sort,
    pub filter: String,
    pub filtering: bool,
    /// The candidate table, the best prices and the coverage, as printed.
    pub notes: Vec<Block<'static>>,
    pub notes_scroll: usize,
    pub auto: bool,
    pub next_due_ms: f64,
    pub follow: FollowState,
    pub last_round: Option<String>,
    pub from_here: bool,
    pub cargo: Option<i64>,
    pub rounding: bool,
}

impl Results {
    /// The rows the filter and sort leave, as indices into `rows`.
    pub(crate) fn visible(&self) -> Vec<usize> {
        let needle = edm_core::js::text::js_trim(&self.filter).to_lowercase();
        let mut indices: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                needle.is_empty()
                    || row.card.path().to_lowercase().contains(&needle)
                    || row.card.cargo().to_lowercase().contains(&needle)
                    || row
                        .card
                        .legs
                        .iter()
                        .any(|leg| leg.from_system.to_lowercase().contains(&needle) || leg.to_system.to_lowercase().contains(&needle))
            })
            .map(|(index, _)| index)
            .collect();
        let rows = &self.rows;
        indices.sort_by(|a, b| {
            let (a, b) = (&rows[*a], &rows[*b]);
            match self.sort {
                Sort::Rank => a.rank.cmp(&b.rank),
                Sort::Profit => b.card.profit.cmp(&a.card.profit),
                Sort::Distance => {
                    let flown = |card: &RouteCard| card.legs.iter().map(|l| l.distance_ly).sum::<f64>();
                    flown(&a.card).total_cmp(&flown(&b.card))
                }
                Sort::Approach => a
                    .card
                    .approach_ly
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(&b.card.approach_ly.unwrap_or(f64::INFINITY)),
                Sort::Time => a.card.first_lap_millis.cmp(&b.card.first_lap_millis),
            }
        });
        indices
    }

    pub(crate) fn selected_row(&self) -> Option<&RouteRow> {
        let visible = self.visible();
        visible.get(self.selected).map(|index| &self.rows[*index])
    }

    fn stations_of(&self, card: &RouteCard) -> Vec<ArdentStation> {
        card.market_ids
            .iter()
            .filter_map(|id| self.stations.iter().find(|station| station.market_id == *id))
            .cloned()
            .collect()
    }
}

/// A pinned route.
#[derive(Clone, Debug)]
pub(crate) struct Pin {
    pub key: PinKey,
    pub label: String,
    pub argv: Vec<String>,
    pub stations: Vec<ArdentStation>,
    pub pinned_at_ms: f64,
    /// The route as last priced, from the search or a refresh.
    pub card: Option<RouteCard>,
    pub state: Option<PinState>,
    pub last: Option<LastKnown>,
    /// `(refreshed at, credits per hour)`, newest last, bounded.
    pub history: Vec<(f64, i64)>,
    pub next_due_ms: f64,
    pub refreshing: bool,
    pub unpriced_since_ms: Option<f64>,
}

const HISTORY_LIMIT: usize = 120;

impl Pin {
    pub(crate) fn restored(
        key: PinKey,
        label: String,
        argv: Vec<String>,
        stations: Vec<ArdentStation>,
        pinned_at_ms: f64,
        last: Option<LastKnown>,
    ) -> Self {
        let unpriced_since_ms = last
            .as_ref()
            .filter(|last| last.unpriced.is_some())
            .map(|last| last.refreshed_at_ms);
        Self {
            key,
            label,
            argv,
            stations,
            pinned_at_ms,
            card: None,
            state: None,
            last,
            history: Vec::new(),
            next_due_ms: 0.0,
            refreshing: false,
            unpriced_since_ms,
        }
    }

    fn from_card(card: &RouteCard, argv: Vec<String>, stations: Vec<ArdentStation>, now_ms: f64) -> Self {
        Self {
            key: card.key.clone(),
            label: format!("{} ({})", card.path(), card.cargo()),
            argv,
            stations,
            pinned_at_ms: now_ms,
            card: Some(card.clone()),
            state: None,
            last: Some(LastKnown {
                per_hour: card.per_hour,
                profit: card.profit,
                refreshed_at_ms: now_ms,
                unpriced: None,
            }),
            history: vec![(now_ms, card.per_hour)],
            next_due_ms: now_ms,
            refreshing: false,
            unpriced_since_ms: None,
        }
    }

    /// The rate to show, from whichever source is newest.
    pub(crate) fn per_hour(&self) -> Option<i64> {
        self.card
            .as_ref()
            .map(|card| card.per_hour)
            .or_else(|| self.last.as_ref().map(|last| last.per_hour))
    }

    pub(crate) fn unpriced_reason(&self) -> Option<&str> {
        self.state
            .as_ref()
            .and_then(|state| state.unpriced_reason.as_deref())
            .or_else(|| self.last.as_ref().and_then(|last| last.unpriced.as_deref()))
    }
}

/// The disposal plan on screen.
pub(crate) struct SellView {
    pub argv: Vec<String>,
    pub data: Option<Box<SellSearch>>,
    pub blocks: Vec<Block<'static>>,
    pub commands: Vec<String>,
    pub aboard: String,
    pub auto: bool,
    pub next_due_ms: f64,
    pub follow: FollowState,
    pub last_round: Option<String>,
    pub sold_out: bool,
    pub rounding: bool,
    pub scroll: usize,
}

/// What is running.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Jobs {
    pub active: Option<String>,
    pub aux: bool,
    /// The solver's latest word, for the results header while a survey solves.
    pub solving: Option<String>,
    pub solve_started_ms: f64,
    pub solve_line_ms: f64,
}

/// What completion draws on.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Completion {
    pub catalogue: Vec<String>,
    pub nearby: Vec<(String, f64)>,
    pub stations: Vec<StationMatch>,
    pub station_query: String,
    /// A station search waiting for the debounce.
    pub pending: Option<String>,
    pub last_edit_ms: f64,
    pub open: bool,
    pub selected: usize,
    pub items: Vec<Candidate>,
}

/// The most log lines kept.
const LOG_LIMIT: usize = 2_000;
/// The station search waits this long after the last key.
const DEBOUNCE_MS: f64 = 250.0;
/// Routes are re-read this many times less often than pins.
const RESULTS_INTERVAL_FACTOR: f64 = 2.0;

/// Everything on screen.
pub(crate) struct AppState {
    pub screen: Screen,
    pub previous: Vec<Screen>,
    pub search: SearchForm,
    pub results: Option<Results>,
    pub pins: Vec<Pin>,
    pub pins_selected: usize,
    /// Which pin the detail screen shows.
    pub detail: usize,
    pub sell: Option<SellView>,
    pub log: VecDeque<LogLine>,
    pub log_strip: bool,
    /// Lines scrolled up from the bottom of the log pane.
    pub log_scroll: usize,
    pub modal: Option<Modal>,
    pub gate_reply: Option<async_channel::Sender<bool>>,
    pub size: (u16, u16),
    pub now_ms: f64,
    /// Requests sent this session, against the ceiling.
    pub spent: usize,
    pub ceiling: f64,
    pub refresh_seconds: f64,
    pub jobs: Jobs,
    pub journal: Option<Box<CommanderState>>,
    pub completion: Completion,
    /// The last Ctrl-C, for the two-press quit while a job runs.
    pub quit_armed_ms: f64,
    pub detail_scroll: usize,
}

impl AppState {
    pub(crate) fn new(ceiling: f64, refresh_seconds: f64) -> Self {
        Self {
            screen: Screen::Search,
            previous: Vec::new(),
            search: SearchForm::default(),
            results: None,
            pins: Vec::new(),
            pins_selected: 0,
            detail: 0,
            sell: None,
            log: VecDeque::new(),
            log_strip: false,
            log_scroll: 0,
            modal: None,
            gate_reply: None,
            size: (80, 24),
            now_ms: 0.0,
            spent: 0,
            ceiling,
            refresh_seconds,
            jobs: Jobs::default(),
            journal: None,
            completion: Completion::default(),
            quit_armed_ms: f64::NEG_INFINITY,
            detail_scroll: 0,
        }
    }

    /// Whether plain characters are text on the current screen.
    pub(crate) fn typing(&self) -> bool {
        if self.modal.is_some() {
            return false;
        }
        match self.screen {
            Screen::Search => self.search.focused().is_text(),
            Screen::Results => self.results.as_ref().is_some_and(|results| results.filtering),
            _ => false,
        }
    }

    pub(crate) fn log(&mut self, stream: Stream, text: impl Into<String>) {
        self.log.push_back(LogLine {
            stream,
            text: text.into(),
        });
        while self.log.len() > LOG_LIMIT {
            self.log.pop_front();
        }
    }

    pub(crate) fn go(&mut self, screen: Screen) {
        if screen != self.screen {
            self.previous.push(self.screen);
            self.screen = screen;
        }
    }

    fn back(&mut self) {
        if let Some(screen) = self.previous.pop() {
            self.screen = screen;
        }
    }

    /// The ship's system and berth, in words, for the status bar.
    pub(crate) fn ship_line(&self) -> String {
        let Some(state) = &self.journal else {
            return "no journal".to_owned();
        };
        let system = state
            .current_system
            .as_ref()
            .map_or("?", |seen| seen.value.name.as_str());
        let berth = state
            .docking
            .as_ref()
            .filter(|docking| docking.value.docked)
            .and_then(|docking| docking.value.station_name.clone());
        let cargo = match (state.cargo.used_value(), state.cargo.capacity_value()) {
            (Some(used), Some(capacity)) => format!(" cargo {used}/{capacity} t"),
            (Some(used), None) => format!(" cargo {used} t"),
            _ => String::new(),
        };
        match berth {
            Some(berth) => format!("{system}, docked at {berth}{cargo}"),
            None => format!("{system}{cargo}"),
        }
    }

    // --- events -----------------------------------------------------------

    /// Apply one event.
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per event, and the events are the whole protocol"
    )]
    pub(crate) fn reduce(&mut self, event: Event, now_ms: f64) -> Vec<Effect> {
        self.now_ms = now_ms;
        match event {
            Event::Tick => self.tick(),
            Event::Log { stream, line } => {
                self.log(stream, line);
                Vec::new()
            }
            Event::Input(crossterm::event::Event::Resize(width, height)) => {
                self.size = (width, height);
                Vec::new()
            }
            Event::Input(crossterm::event::Event::Key(key)) => {
                match super::keys::action(&key, self.screen, self.typing()) {
                    Some(action) => self.act(action),
                    None => Vec::new(),
                }
            }
            Event::Input(crossterm::event::Event::Paste(text)) => {
                if self.typing() {
                    for c in text.chars() {
                        self.act(Action::Type(c));
                    }
                }
                Vec::new()
            }
            Event::Input(_) => Vec::new(),
            Event::Gate { gated, reply } => {
                let mut text = String::new();
                let width = usize::from(self.size.0.saturating_sub(6)).clamp(48, 100);
                write_blocks(&mut text, &gated.plan, width, edm_core::js::text::Metric::Display);
                self.modal = Some(Modal::Confirm {
                    lines: text.lines().map(ToOwned::to_owned).collect(),
                    message: edm_core::spend::confirmation_message(&gated.estimate),
                });
                self.gate_reply = Some(reply);
                Vec::new()
            }
            Event::Solving(progress) => {
                self.jobs.solving = Some(edm_route::view::progress(progress));
                Vec::new()
            }
            Event::QuickDone { search, argv } => {
                self.take_quick(search, argv);
                Vec::new()
            }
            Event::SurveyDone { search, argv } => {
                self.take_survey(&search, argv);
                Vec::new()
            }
            Event::SellDone { search, argv } => {
                self.take_sell(search, argv);
                Vec::new()
            }
            Event::QuickRound {
                search,
                argv,
                outcome,
            } => {
                self.after_quick_round(search, argv, &outcome);
                Vec::new()
            }
            Event::SellRound {
                search,
                argv,
                outcome,
            } => {
                self.after_sell_round(search, argv, &outcome);
                Vec::new()
            }
            Event::Repriced { key, state } => {
                self.after_reprice(&key, *state);
                vec![Effect::SavePins]
            }
            Event::Journal(state) => {
                for warning in &state.warnings {
                    self.log(Stream::Stderr, format!("journal: {}", warning.message));
                }
                let system_changed = self
                    .journal
                    .as_ref()
                    .and_then(|old| old.current_system.as_ref().map(|s| s.value.name.clone()))
                    != state.current_system.as_ref().map(|s| s.value.name.clone());
                let system = state.current_system.as_ref().map(|s| s.value.name.clone());
                self.journal = Some(state);
                if system_changed && self.completion.nearby.is_empty() {
                    return vec![Effect::StartAux(AuxSpec::Warmup { system })];
                }
                Vec::new()
            }
            Event::Catalogue(ids) => {
                self.completion.catalogue = ids;
                Vec::new()
            }
            Event::Nearby(systems) => {
                self.completion.nearby = systems
                    .into_iter()
                    .map(|system| (system.name, system.distance))
                    .collect();
                Vec::new()
            }
            Event::StationMatches { query, matches } => {
                if self.completion.pending.as_deref() == Some(query.as_str()) {
                    self.completion.pending = None;
                }
                self.completion.station_query = query;
                self.completion.stations = matches;
                self.refresh_completion();
                Vec::new()
            }
            Event::Stopped => {
                self.jobs.active = None;
                self.jobs.solving = None;
                self.gate_reply = None;
                self.clear_rounding();
                self.log(Stream::Stderr, "stopped: nothing was sent");
                Vec::new()
            }
            Event::Finished { spent } => {
                self.spent = spent.requests;
                self.jobs.active = None;
                self.jobs.solving = None;
                self.gate_reply = None;
                Vec::new()
            }
            Event::Error(message) => {
                self.jobs.active = None;
                self.jobs.solving = None;
                self.gate_reply = None;
                self.clear_rounding();
                self.log(Stream::Stderr, format!("error: {message}"));
                if self.screen == Screen::Search {
                    self.search.status = Some(message);
                } else {
                    self.modal = Some(Modal::Message(message));
                }
                Vec::new()
            }
            Event::AuxError(message) => {
                self.jobs.aux = false;
                self.completion.pending = None;
                self.log(Stream::Stderr, format!("lookup: {message}"));
                Vec::new()
            }
        }
    }

    /// A round that failed leaves its instance where it was.
    fn clear_rounding(&mut self) {
        if let Some(results) = self.results.as_mut() {
            results.rounding = false;
            for row in &mut results.rows {
                if row.status == LiveStatus::Verifying {
                    row.status = LiveStatus::Cached;
                }
            }
        }
        if let Some(sell) = self.sell.as_mut() {
            sell.rounding = false;
        }
        for pin in &mut self.pins {
            pin.refreshing = false;
        }
    }

    fn take_quick(&mut self, search: Box<QuickSearch>, argv: Vec<String>) {
        let cargo = search.rank_config.cargo.map(|tons| tons as i64);
        let rows: Vec<RouteRow> = search
            .ranked
            .routes()
            .iter()
            .enumerate()
            .map(|(n, route)| {
                let card = RouteCard::of(
                    route,
                    &search.ranked.markets,
                    &search.ranked.commodities,
                    search.origin,
                    cargo,
                );
                let status = if card
                    .market_ids
                    .iter()
                    .all(|id| search.live.contains(&id.to_bits()))
                {
                    LiveStatus::Live
                } else {
                    LiveStatus::Cached
                };
                RouteRow {
                    rank: n + 1,
                    card,
                    status,
                }
            })
            .collect();
        let mut notes = search.candidate_blocks.clone();
        notes.extend(search.live_blocks.clone());
        notes.extend(edm_core::render::views::route_coverage(&search.coverage));
        let from_here = search.rank_config.from_here;
        self.results = Some(Results {
            argv,
            quick: true,
            stations: search.stations.clone(),
            data: Some(search),
            rows,
            selected: 0,
            sort: Sort::Rank,
            filter: String::new(),
            filtering: false,
            notes,
            notes_scroll: 0,
            auto: false,
            next_due_ms: self.now_ms + self.refresh_seconds * RESULTS_INTERVAL_FACTOR * 1_000.0,
            follow: FollowState::default(),
            last_round: None,
            from_here,
            cargo,
            rounding: false,
        });
        self.go(Screen::Results);
    }

    fn take_survey(&mut self, search: &SurveySearch, argv: Vec<String>) {
        let rows: Vec<RouteRow> = search
            .ranked
            .routes()
            .iter()
            .enumerate()
            .map(|(n, route)| RouteRow {
                rank: n + 1,
                card: RouteCard::of(
                    route,
                    &search.ranked.markets,
                    &search.ranked.commodities,
                    search.origin,
                    None,
                ),
                status: LiveStatus::Live,
            })
            .collect();
        let mut notes: Vec<Block<'static>> = crate::cmd::route::crossing_notes(&search.ranked.crossing)
            .into_iter()
            .map(Block::Line)
            .collect();
        notes.extend(edm_core::render::views::route_coverage(&search.coverage));
        self.results = Some(Results {
            argv,
            quick: false,
            stations: search.stations.clone(),
            data: None,
            rows,
            selected: 0,
            sort: Sort::Rank,
            filter: String::new(),
            filtering: false,
            notes,
            notes_scroll: 0,
            auto: false,
            next_due_ms: f64::INFINITY,
            follow: FollowState::default(),
            last_round: None,
            from_here: false,
            cargo: None,
            rounding: false,
        });
        self.go(Screen::Results);
    }

    fn sell_blocks(search: &SellSearch) -> (Vec<Block<'static>>, Vec<String>) {
        let solved = &search.solved;
        let Some(best) = solved.plans.first() else {
            return (Vec::new(), Vec::new());
        };
        let geometry = edm_route::time::Geometry::new(
            &solved.markets,
            crate::cmd::route::time_model(&search.route_config),
        );
        let alternative = crate::cmd::sell::most_credits(best, &solved.plans);
        let mut blocks = crate::cmd::sell::plan_blocks(
            best,
            alternative,
            &solved.markets,
            &solved.commodities,
            &geometry,
            search.origin,
            solved.bar,
        );
        let commands_blocks = crate::cmd::sell::sell_trade_commands(
            best,
            alternative,
            &solved.markets,
            &solved.commodities,
        );
        let commands: Vec<String> = commands_blocks
            .iter()
            .filter_map(|block| match block {
                Block::Raw(line) if line.contains("edm trade") => Some(edm_core::js::text::js_trim(line).to_owned()),
                _ => None,
            })
            .collect();
        blocks.extend(commands_blocks);
        (blocks, commands)
    }

    fn take_sell(&mut self, search: Box<SellSearch>, argv: Vec<String>) {
        let (blocks, commands) = Self::sell_blocks(&search);
        let aboard = crate::cmd::sell::describe_stacks(&search.hold);
        self.sell = Some(SellView {
            argv,
            data: Some(search),
            blocks,
            commands,
            aboard,
            auto: false,
            next_due_ms: self.now_ms + self.refresh_seconds * 1_000.0,
            follow: FollowState::default(),
            last_round: None,
            sold_out: false,
            rounding: false,
            scroll: 0,
        });
        self.go(Screen::Sell);
    }

    fn after_quick_round(&mut self, search: Box<QuickSearch>, argv: Vec<String>, outcome: &RoundOutcome) {
        let Some(results) = self.results.as_mut() else { return };
        results.rounding = false;
        results.argv = argv;
        results.follow.begin_round();
        let round = results.follow.round;
        if outcome.moved_away {
            results.auto = false;
            results.last_round = Some(
                "you have moved: every route in this list departs from the station you were docked at, so the list no longer applies. Search again from where you are now"
                    .to_owned(),
            );
            results.data = Some(search);
            return;
        }
        let cargo = results.cargo;
        let previous = std::mem::take(&mut results.rows);
        let mut rows: Vec<RouteRow> = search
            .ranked
            .routes()
            .iter()
            .enumerate()
            .map(|(n, route)| RouteRow {
                rank: n + 1,
                card: RouteCard::of(
                    route,
                    &search.ranked.markets,
                    &search.ranked.commodities,
                    search.origin,
                    cargo,
                ),
                status: LiveStatus::Live,
            })
            .collect();
        // A route the round could not price stays on the list, marked, so it
        // can come back when its market restocks \[C43\].
        for old in previous {
            if !rows.iter().any(|row| row.card.key == old.card.key) {
                rows.push(RouteRow {
                    rank: old.rank,
                    card: old.card,
                    status: LiveStatus::Unpriced,
                });
            }
        }
        let priced = search.ranked.routes().len();
        results.rows = rows;
        results.selected = results.selected.min(results.visible().len().saturating_sub(1));
        results.last_round = Some(format!(
            "round {}: {} markets re-read, {} requests, {} of {} routes still priced{}",
            edm_core::js::format_integer(round as f64),
            edm_core::js::format_integer(outcome.verified.markets as f64),
            edm_core::js::format_integer(outcome.requests as f64),
            edm_core::js::format_integer(priced as f64),
            edm_core::js::format_integer(search.shortlist.len() as f64),
            if outcome.tripped { " (the rate limiter tripped)" } else { "" },
        ));
        if let Some(barren) = results.follow.record(priced > 0) {
            results.auto = false;
            results.last_round = Some(format!(
                "the whole shortlist has been unpriced for {barren} rounds: every route in it has lost a side. Search again"
            ));
        }
        results.next_due_ms = self.now_ms + self.refresh_seconds * RESULTS_INTERVAL_FACTOR * 1_000.0;
        results.data = Some(search);
        // A pinned route that is on this list shows its fresh card at once.
        for pin in &mut self.pins {
            if let Some(row) = results.rows.iter().find(|row| row.card.key == pin.key && row.status == LiveStatus::Live) {
                pin.card = Some(row.card.clone());
            }
        }
    }

    fn after_sell_round(&mut self, search: Box<SellSearch>, argv: Vec<String>, outcome: &SellRound) {
        let Some(view) = self.sell.as_mut() else { return };
        view.rounding = false;
        view.argv = argv;
        view.follow.begin_round();
        for stack in &outcome.newly_unplanned {
            self.log.push_back(LogLine {
                stream: Stream::Stderr,
                text: format!(
                    "{} t of {} is aboard but not in this plan: its buyers were never nominated",
                    stack.tons, stack.symbol
                ),
            });
        }
        if outcome.sold_out {
            view.sold_out = true;
            view.auto = false;
            view.last_round = Some("the hold is empty: everything is sold".to_owned());
            view.data = Some(search);
            return;
        }
        view.aboard = crate::cmd::sell::describe_stacks(&search.hold);
        let round = view.follow.round;
        match &outcome.plan {
            Ok(()) => {
                view.follow.record(true);
                let (blocks, commands) = Self::sell_blocks(&search);
                view.blocks = blocks;
                view.commands = commands;
                view.last_round = Some(format!(
                    "round {}: {} of {} buyers re-read, {} requests, {} aboard",
                    round,
                    outcome.read,
                    search.keep.len(),
                    outcome.requests,
                    view.aboard
                ));
            }
            Err(reason) => {
                view.last_round = Some(format!("round {round}: {reason}"));
                if let Some(barren) = view.follow.record(false) {
                    view.auto = false;
                    view.last_round = Some(format!(
                        "nothing could be planned for {barren} rounds: every nominated buyer has gone. Search again"
                    ));
                }
            }
        }
        view.next_due_ms = self.now_ms + self.refresh_seconds * 1_000.0;
        view.data = Some(search);
    }

    fn after_reprice(&mut self, key: &PinKey, state: PinState) {
        let now_ms = self.now_ms;
        let refresh_ms = self.refresh_seconds * 1_000.0;
        let Some(pin) = self.pins.iter_mut().find(|pin| pin.key == *key) else { return };
        pin.refreshing = false;
        pin.next_due_ms = now_ms + refresh_ms;
        if let Some(card) = &state.route {
            pin.card = Some(card.clone());
            pin.history.push((state.refreshed_at_ms, card.per_hour));
            if pin.history.len() > HISTORY_LIMIT {
                pin.history.remove(0);
            }
            pin.last = Some(LastKnown {
                per_hour: card.per_hour,
                profit: card.profit,
                refreshed_at_ms: state.refreshed_at_ms,
                unpriced: None,
            });
            pin.unpriced_since_ms = None;
        } else {
            if pin.unpriced_since_ms.is_none() {
                pin.unpriced_since_ms = Some(state.refreshed_at_ms);
            }
            if let Some(last) = pin.last.as_mut() {
                last.refreshed_at_ms = state.refreshed_at_ms;
                last.unpriced.clone_from(&state.unpriced_reason);
            } else {
                pin.last = Some(LastKnown {
                    per_hour: 0,
                    profit: 0,
                    refreshed_at_ms: state.refreshed_at_ms,
                    unpriced: state.unpriced_reason.clone(),
                });
            }
        }
        pin.state = Some(state);
    }

    // --- the clock ----------------------------------------------------------

    /// Debounce, then schedule.
    fn tick(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        // The station search waits for the typing to stop.
        if let Some(query) = self.completion.pending.clone()
            && !self.jobs.aux
            && self.now_ms - self.completion.last_edit_ms >= DEBOUNCE_MS
            && query != self.completion.station_query
        {
            self.jobs.aux = true;
            effects.push(Effect::StartAux(AuxSpec::StationSearch(query)));
        }
        if self.jobs.active.is_some() {
            return effects;
        }
        if self.spent as f64 >= self.ceiling {
            return effects;
        }
        if let Some(job) = self.due_job() {
            effects.push(Effect::StartJob(job));
        }
        effects
    }

    /// The earliest due refresh: pins first, then the sale, then the list.
    fn due_job(&mut self) -> Option<JobSpec> {
        let now_ms = self.now_ms;
        if let Some(index) = self
            .pins
            .iter()
            .enumerate()
            .filter(|(_, pin)| !pin.refreshing && pin.next_due_ms <= now_ms)
            .min_by(|a, b| a.1.next_due_ms.total_cmp(&b.1.next_due_ms))
            .map(|(index, _)| index)
        {
            return self.reprice_job(index);
        }
        if let Some(sell) = self.sell.as_mut()
            && sell.auto
            && !sell.sold_out
            && sell.next_due_ms <= now_ms
            && let Some(data) = sell.data.take()
        {
            sell.rounding = true;
            return Some(JobSpec::SellRound {
                search: data,
                argv: sell.argv.clone(),
            });
        }
        if let Some(results) = self.results.as_mut()
            && results.quick
            && results.auto
            && results.next_due_ms <= now_ms
            && let Some(data) = results.data.take()
        {
            results.rounding = true;
            for row in &mut results.rows {
                row.status = LiveStatus::Verifying;
            }
            return Some(JobSpec::QuickRound {
                search: data,
                argv: results.argv.clone(),
                stop_if_moved: results.from_here,
            });
        }
        None
    }

    fn reprice_job(&mut self, index: usize) -> Option<JobSpec> {
        let journal = self.journal.clone();
        let pin = self.pins.get_mut(index)?;
        pin.refreshing = true;
        Some(JobSpec::Reprice(Box::new(PinJob {
            key: pin.key.clone(),
            label: pin.label.clone(),
            argv: pin.argv.clone(),
            stations: pin.stations.clone(),
            commander: journal,
        })))
    }

    // --- keys ----------------------------------------------------------------

    /// Apply one action.
    pub(crate) fn act(&mut self, action: Action) -> Vec<Effect> {
        if self.modal.is_some() {
            return self.act_modal(action);
        }
        match action {
            Action::Quit => {
                if self.jobs.active.is_some() && self.now_ms - self.quit_armed_ms > 2_000.0 {
                    self.quit_armed_ms = self.now_ms;
                    self.modal = Some(Modal::Message(format!(
                        "{} is still running. Press Ctrl-C again within two seconds to quit anyway.",
                        self.jobs.active.as_deref().unwrap_or("a job")
                    )));
                    return Vec::new();
                }
                vec![Effect::Quit]
            }
            Action::Help => {
                self.modal = Some(Modal::Help);
                Vec::new()
            }
            Action::ToggleLogStrip => {
                self.log_strip = !self.log_strip;
                Vec::new()
            }
            Action::Go(screen) => {
                self.go(screen);
                Vec::new()
            }
            Action::Back => {
                if self.screen == Screen::Search && self.completion.open {
                    self.completion.open = false;
                    return Vec::new();
                }
                if self.screen == Screen::Results
                    && let Some(results) = self.results.as_mut()
                    && results.filtering
                {
                    results.filtering = false;
                    return Vec::new();
                }
                if self.jobs.active.is_some() && self.screen == Screen::Results && self.results.is_none() {
                    return vec![Effect::CancelJob];
                }
                self.back();
                Vec::new()
            }
            other => match self.screen {
                Screen::Search => self.act_search(other),
                Screen::Results => self.act_results(other),
                Screen::Detail => self.act_detail(other),
                Screen::Pins => self.act_pins(other),
                Screen::Sell => self.act_sell(other),
                Screen::Log => {
                    self.act_log(other);
                    Vec::new()
                }
            },
        }
    }

    fn act_modal(&mut self, action: Action) -> Vec<Effect> {
        let confirm = matches!(self.modal, Some(Modal::Confirm { .. }));
        match action {
            Action::Enter | Action::Type('y' | 'Y') if confirm => {
                self.modal = None;
                vec![Effect::AnswerGate(true)]
            }
            Action::Back | Action::Type('n' | 'N') | Action::Quit if confirm => {
                self.modal = None;
                vec![Effect::AnswerGate(false)]
            }
            Action::Quit if self.now_ms - self.quit_armed_ms <= 2_000.0 => vec![Effect::Quit],
            Action::Back | Action::Enter | Action::Help | Action::Quit | Action::Space => {
                self.modal = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per key the form answers"
    )]
    fn act_search(&mut self, action: Action) -> Vec<Effect> {
        let form = &mut self.search;
        let mut edited = false;
        match action {
            Action::Up | Action::Down if self.completion.open => {
                let len = self.completion.items.len();
                if len > 0 {
                    let delta: isize = if action == Action::Up { -1 } else { 1 };
                    self.completion.selected =
                        (self.completion.selected as isize + delta).rem_euclid(len as isize) as usize;
                }
                return Vec::new();
            }
            Action::Enter if self.completion.open => {
                if let Some(chosen) = self.completion.items.get(self.completion.selected).cloned() {
                    let field = form.focused_mut();
                    if field.completes == Completes::Commodities {
                        let (start, _) = autocomplete::last_token(&field.text);
                        field.text.truncate(start);
                        if start > 0 && !field.text.ends_with(' ') {
                            field.text.push(' ');
                        }
                        field.text.push_str(&chosen.insert);
                    } else {
                        field.text = chosen.insert;
                    }
                }
                self.completion.open = false;
                return Vec::new();
            }
            Action::Up | Action::Previous => form.step(-1),
            Action::Down | Action::Next => form.step(1),
            Action::Left | Action::Right => {
                let delta = if action == Action::Left { -1 } else { 1 };
                let field = form.focused_mut();
                match field.kind {
                    FieldKind::Choice(options) => {
                        field.choice =
                            (field.choice as isize + delta).rem_euclid(options.len() as isize) as usize;
                    }
                    FieldKind::Switch => field.on = !field.on,
                    FieldKind::Text | FieldKind::Number => {}
                }
            }
            Action::Space => {
                let field = form.focused_mut();
                match field.kind {
                    FieldKind::Switch => field.on = !field.on,
                    FieldKind::Choice(options) => field.choice = (field.choice + 1) % options.len(),
                    FieldKind::Text | FieldKind::Number => {
                        field.text.push(' ');
                        edited = true;
                    }
                }
            }
            Action::Type(c) => {
                let field = form.focused_mut();
                if field.is_text() {
                    field.text.push(c);
                    edited = true;
                }
            }
            Action::Backspace => {
                form.focused_mut().text.pop();
                edited = true;
            }
            Action::Delete | Action::Home => {
                form.focused_mut().text.clear();
                edited = true;
            }
            Action::PageUp => form.cycle_mode(-1),
            Action::PageDown | Action::End => form.cycle_mode(1),
            Action::Enter => {
                if self.jobs.active.is_some() {
                    form.status = Some(format!(
                        "{} is still running; Esc on the results screen cancels it",
                        self.jobs.active.as_deref().unwrap_or("a job")
                    ));
                    return Vec::new();
                }
                let argv = form.argv();
                self.completion.open = false;
                form.status = Some(format!("running: edm {}", argv.join(" ")));
                self.jobs.active = Some(format!("edm {}", argv.join(" ")));
                if form.mode != Mode::Sell {
                    self.go(Screen::Results);
                }
                return vec![Effect::SaveSearch(argv.clone()), Effect::StartJob(JobSpec::Search(argv))];
            }
            Action::Quit
            | Action::Help
            | Action::ToggleLogStrip
            | Action::Go(_)
            | Action::Back => {}
        }
        if edited {
            self.completion.last_edit_ms = self.now_ms;
            self.refresh_completion();
        } else {
            self.completion.open = false;
        }
        Vec::new()
    }

    /// Rebuild the completion popup for the focused field.
    fn refresh_completion(&mut self) {
        let field = self.search.focused();
        let (query, candidates): (String, Vec<Candidate>) = match field.completes {
            Completes::Nothing => {
                self.completion.open = false;
                return;
            }
            Completes::Places => (field.text.clone(), self.place_candidates()),
            Completes::Commodities => {
                let (_, token) = autocomplete::last_token(&field.text);
                (
                    token.to_owned(),
                    self.completion
                        .catalogue
                        .iter()
                        .map(|id| Candidate {
                            label: edm_route::view::readable(id),
                            insert: id.clone(),
                            kind: Kind::Commodity,
                            hint: edm_core::ardent::commodity_category(id).unwrap_or("").to_owned(),
                            recency: 0,
                        })
                        .collect(),
                )
            }
            Completes::Categories => (
                field.text.clone(),
                edm_core::ardent::known_categories()
                    .iter()
                    .map(|name| Candidate {
                        label: (*name).to_owned(),
                        insert: (*name).to_owned(),
                        kind: Kind::Category,
                        hint: String::new(),
                        recency: 0,
                    })
                    .collect(),
            ),
        };
        if field.completes == Completes::Places {
            let trimmed = edm_core::js::text::js_trim(&query).to_owned();
            if trimmed.chars().count() >= 3 && trimmed != self.completion.station_query {
                self.completion.pending = Some(trimmed);
            } else {
                self.completion.pending = None;
            }
        }
        let items = autocomplete::rank(&query, &candidates, 8);
        self.completion.open = !edm_core::js::text::js_trim(&query).is_empty() && !items.is_empty();
        self.completion.selected = 0;
        self.completion.items = items;
    }

    fn place_candidates(&self) -> Vec<Candidate> {
        let mut out = Vec::new();
        if let Some(state) = &self.journal {
            for (n, seen) in state.visited_systems.iter().enumerate() {
                out.push(Candidate {
                    label: seen.name.clone(),
                    insert: seen.name.clone(),
                    kind: Kind::System,
                    hint: "visited".to_owned(),
                    recency: 1_000 - n.min(999) as u32,
                });
            }
            for (n, seen) in state.visited_stations.iter().enumerate() {
                out.push(Candidate {
                    label: seen.station.clone(),
                    insert: seen.station.clone(),
                    kind: Kind::Station,
                    hint: seen.system.clone().unwrap_or_else(|| "docked".to_owned()),
                    recency: 900 - n.min(899) as u32,
                });
            }
        }
        for (name, distance) in &self.completion.nearby {
            out.push(Candidate {
                label: name.clone(),
                insert: name.clone(),
                kind: Kind::System,
                hint: format!("{} Ly", edm_core::js::format_integer(*distance)),
                recency: 100,
            });
        }
        for found in &self.completion.stations {
            out.push(Candidate {
                label: found.station_name.clone(),
                insert: found.station_name.clone(),
                kind: Kind::Station,
                hint: found.system_name.clone(),
                recency: 50,
            });
        }
        out
    }

    fn act_results(&mut self, action: Action) -> Vec<Effect> {
        let now_ms = self.now_ms;
        let refresh_seconds = self.refresh_seconds;
        let Some(results) = self.results.as_mut() else {
            if action == Action::Enter {
                self.go(Screen::Search);
            }
            return Vec::new();
        };
        if results.filtering {
            match action {
                Action::Type(c) => results.filter.push(c),
                Action::Backspace => {
                    results.filter.pop();
                }
                Action::Enter | Action::Back => results.filtering = false,
                _ => {}
            }
            results.selected = results.selected.min(results.visible().len().saturating_sub(1));
            return Vec::new();
        }
        let visible = results.visible().len();
        match action {
            Action::Up => results.selected = results.selected.saturating_sub(1),
            Action::Down => results.selected = (results.selected + 1).min(visible.saturating_sub(1)),
            Action::PageUp => results.notes_scroll = results.notes_scroll.saturating_sub(10),
            Action::PageDown => results.notes_scroll += 10,
            Action::Home => results.selected = 0,
            Action::End => results.selected = visible.saturating_sub(1),
            Action::Type('/') => results.filtering = true,
            Action::Type('r') => results.sort = Sort::Rank,
            Action::Type('P') => results.sort = Sort::Profit,
            Action::Type('d') => results.sort = Sort::Distance,
            Action::Type('s') => results.sort = Sort::Approach,
            Action::Type('t') => results.sort = Sort::Time,
            Action::Type('f') => {
                if results.quick {
                    results.auto = !results.auto;
                    if results.auto {
                        results.next_due_ms = now_ms;
                    }
                }
            }
            Action::Type('R') => {
                if results.quick {
                    results.next_due_ms = now_ms;
                    results.auto = results.auto || results.data.is_some();
                    if !results.auto {
                        results.next_due_ms = now_ms;
                    }
                    let _ = refresh_seconds;
                }
            }
            Action::Type('c') => {
                if let Some(row) = results.selected_row() {
                    return vec![Effect::Copy(row.card.commands.join("\n"))];
                }
            }
            Action::Type('p') | Action::Enter => {
                let Some(row) = results.selected_row().cloned() else {
                    return Vec::new();
                };
                let argv = results.argv.clone();
                let stations = results.stations_of(&row.card);
                if let Some(index) = self.pins.iter().position(|pin| pin.key == row.card.key) {
                    if action == Action::Enter {
                        self.detail = index;
                        self.go(Screen::Detail);
                        return Vec::new();
                    }
                    self.pins.remove(index);
                    return vec![Effect::SavePins];
                }
                if stations.len() != row.card.key.stations.len() {
                    self.modal = Some(Modal::Message(
                        "this route's markets are not all known to the search, so it cannot be pinned".to_owned(),
                    ));
                    return Vec::new();
                }
                self.pins.push(Pin::from_card(&row.card, argv, stations, now_ms));
                if action == Action::Enter {
                    self.detail = self.pins.len() - 1;
                    self.go(Screen::Detail);
                }
                return vec![Effect::SavePins];
            }
            _ => {}
        }
        Vec::new()
    }

    fn act_detail(&mut self, action: Action) -> Vec<Effect> {
        let now_ms = self.now_ms;
        if self.pins.is_empty() {
            if action == Action::Enter {
                self.go(Screen::Pins);
            }
            return Vec::new();
        }
        self.detail = self.detail.min(self.pins.len() - 1);
        match action {
            Action::Type('[') | Action::Left => {
                self.detail = (self.detail + self.pins.len() - 1) % self.pins.len();
                self.detail_scroll = 0;
            }
            Action::Type(']') | Action::Right => {
                self.detail = (self.detail + 1) % self.pins.len();
                self.detail_scroll = 0;
            }
            Action::Up => self.detail_scroll = self.detail_scroll.saturating_sub(1),
            Action::Down => self.detail_scroll += 1,
            Action::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            Action::PageDown => self.detail_scroll += 10,
            Action::Type('R') => {
                self.pins[self.detail].next_due_ms = now_ms;
                return vec![Effect::ReadJournal];
            }
            Action::Type('u') => {
                self.pins.remove(self.detail);
                if self.pins.is_empty() {
                    self.back();
                } else {
                    self.detail = self.detail.min(self.pins.len() - 1);
                }
                return vec![Effect::SavePins];
            }
            Action::Type('c') => {
                if let Some(card) = self.pins[self.detail].card.as_ref() {
                    return vec![Effect::Copy(card.commands.join("\n"))];
                }
            }
            Action::Type(digit @ '1'..='9') => {
                let n = digit as usize - '1' as usize;
                if let Some(line) = self.pins[self.detail].card.as_ref().and_then(|card| card.commands.get(n)) {
                    return vec![Effect::Copy(line.clone())];
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn act_pins(&mut self, action: Action) -> Vec<Effect> {
        if self.pins.is_empty() {
            return Vec::new();
        }
        self.pins_selected = self.pins_selected.min(self.pins.len() - 1);
        match action {
            Action::Up => self.pins_selected = self.pins_selected.saturating_sub(1),
            Action::Down => self.pins_selected = (self.pins_selected + 1).min(self.pins.len() - 1),
            Action::Enter => {
                self.detail = self.pins_selected;
                self.detail_scroll = 0;
                self.go(Screen::Detail);
            }
            Action::Type('d') | Action::Delete => {
                self.pins.remove(self.pins_selected);
                self.pins_selected = self.pins_selected.min(self.pins.len().saturating_sub(1));
                return vec![Effect::SavePins];
            }
            Action::Type('R') => {
                self.pins[self.pins_selected].next_due_ms = self.now_ms;
            }
            Action::Type('o') => {
                let argv = self.pins[self.pins_selected].argv.clone();
                self.search.load(&argv);
                self.go(Screen::Search);
            }
            _ => {}
        }
        Vec::new()
    }

    fn act_sell(&mut self, action: Action) -> Vec<Effect> {
        let now_ms = self.now_ms;
        let Some(view) = self.sell.as_mut() else {
            if action == Action::Enter {
                self.search.set_mode(Mode::Sell);
                self.go(Screen::Search);
            }
            return Vec::new();
        };
        match action {
            Action::Up => view.scroll = view.scroll.saturating_sub(1),
            Action::Down => view.scroll += 1,
            Action::PageUp => view.scroll = view.scroll.saturating_sub(10),
            Action::PageDown => view.scroll += 10,
            Action::Type('s') => {
                view.auto = !view.auto;
                if view.auto {
                    view.next_due_ms = now_ms;
                    view.sold_out = false;
                }
            }
            Action::Type('R') => {
                view.auto = true;
                view.sold_out = false;
                view.next_due_ms = now_ms;
            }
            Action::Type('c') => return vec![Effect::Copy(view.commands.join("\n"))],
            _ => {}
        }
        Vec::new()
    }

    fn act_log(&mut self, action: Action) {
        let page = usize::from(self.size.1.saturating_sub(4)).max(1);
        let max = self.log.len().saturating_sub(1);
        match action {
            Action::Up => self.log_scroll = (self.log_scroll + 1).min(max),
            Action::Down => self.log_scroll = self.log_scroll.saturating_sub(1),
            Action::PageUp => self.log_scroll = (self.log_scroll + page).min(max),
            Action::PageDown => self.log_scroll = self.log_scroll.saturating_sub(page),
            Action::Home => self.log_scroll = max,
            Action::End => self.log_scroll = 0,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::plan::Gated;

    #[test]
    fn the_form_spells_the_command_line_it_describes() {
        let mut state = AppState::new(2_000.0, 60.0);
        for c in "Sol".chars() {
            state.act(Action::Type(c));
        }
        state.act(Action::Down);
        for c in "gold, silver".chars() {
            state.act(Action::Type(c));
        }
        assert_eq!(
            state.search.argv(),
            ["route", "Sol", "--item", "gold, silver", "--quick", "5"]
        );
        // Survey mode drops the lookup flags and keeps the rest.
        state.act(Action::PageDown);
        assert_eq!(state.search.mode, Mode::Survey);
        assert_eq!(state.search.argv(), ["route", "Sol", "--item", "gold, silver"]);
        // And the form reloads from what it produced.
        let argv: Vec<String> = ["sell", "--from", "Sol", "--item", "tritium", "--stops", "2", "--carriers"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        state.search.load(&argv);
        assert_eq!(state.search.mode, Mode::Sell);
        assert_eq!(state.search.argv(), argv);
    }

    #[test]
    fn focus_only_lands_on_fields_the_mode_shows() {
        let mut state = AppState::new(2_000.0, 60.0);
        let visible = state.search.visible();
        for _ in 0..visible.len() {
            state.act(Action::Next);
            assert!(state.search.focused().applies(Mode::Quick));
        }
        assert_eq!(state.search.focus, visible[0], "wraps");
        state.act(Action::PageDown);
        state.act(Action::PageDown);
        assert_eq!(state.search.mode, Mode::Sell);
        assert!(state.search.focused().applies(Mode::Sell));
    }

    #[test]
    fn a_modal_swallows_keys_until_it_is_closed() {
        let mut state = AppState::new(2_000.0, 60.0);
        state.act(Action::Help);
        assert_eq!(state.modal, Some(Modal::Help));
        assert!(state.act(Action::Quit).is_empty(), "quit closes the modal instead");
        assert_eq!(state.modal, None);
        assert!(matches!(state.act(Action::Quit).as_slice(), [Effect::Quit]));
    }

    #[test]
    fn the_log_is_bounded_and_scrolls_from_the_bottom() {
        let mut state = AppState::new(2_000.0, 60.0);
        for n in 0..(LOG_LIMIT + 5) {
            state.log(Stream::Stdout, n.to_string());
        }
        assert_eq!(state.log.len(), LOG_LIMIT);
        state.go(Screen::Log);
        state.act(Action::Up);
        assert_eq!(state.log_scroll, 1);
        state.act(Action::End);
        assert_eq!(state.log_scroll, 0);
    }

    /// The gate's modal answers through the reply it was handed, and either
    /// key closes it.
    #[test]
    fn the_gate_modal_answers_yes_or_no() {
        let mut state = AppState::new(2_000.0, 60.0);
        let (reply, answer) = async_channel::bounded::<bool>(1);
        let gated = Gated {
            estimate: edm_core::spend::Estimate::build(
                edm_core::spend::Counts::default(),
                Vec::new(),
                4.0,
                &edm_core::spend::SizePrior::default(),
            ),
            verdict: edm_core::spend::Verdict::NeedsConfirmation,
            plan: vec![Block::Line("plan".to_owned())],
        };
        state.reduce(Event::Gate { gated, reply }, 1.0);
        assert!(matches!(state.modal, Some(Modal::Confirm { .. })));
        let effects = state.act(Action::Type('y'));
        assert!(matches!(effects.as_slice(), [Effect::AnswerGate(true)]));
        assert_eq!(state.modal, None);
        // The loop sends the answer; the reducer only names it.
        assert!(answer.try_recv().is_err());
    }

    /// A pin falls due on its interval, and only one job is scheduled at a
    /// time, pins before anything else.
    #[test]
    fn the_scheduler_runs_the_earliest_pin_first_and_one_job_at_a_time() {
        let mut state = AppState::new(2_000.0, 60.0);
        let key = |n: i64| PinKey {
            kind: edm_route::pin::PinKind::OneWay,
            stations: vec![n, n + 1],
            commodities: vec!["gold".to_owned()],
        };
        for n in [10, 20] {
            let mut pin = Pin::restored(key(n), format!("pin {n}"), vec!["route".to_owned()], Vec::new(), 0.0, None);
            pin.next_due_ms = f64::from(n as i32);
            state.pins.push(pin);
        }
        let effects = state.reduce(Event::Tick, 100.0);
        match effects.as_slice() {
            [Effect::StartJob(JobSpec::Reprice(job))] => assert_eq!(job.label, "pin 10"),
            other => panic!("expected one re-price, got {} effects", other.len()),
        }
        assert!(state.pins[0].refreshing);
        state.jobs.active = Some("re-pricing".to_owned());
        assert!(state.reduce(Event::Tick, 200.0).is_empty(), "one at a time");
        // At the ceiling nothing more is scheduled.
        state.jobs.active = None;
        state.spent = 2_000;
        assert!(state.reduce(Event::Tick, 300.0).is_empty());
    }
}
