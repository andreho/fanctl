//! The `fanctlui` client: it holds the last state served by the `fanctld`
//! daemon, renders it, and sends fan commands to the daemon. It never
//! touches the hardware itself, so the fans stay under the daemon's control
//! even when the TUI is closed.
//!
//! All daemon I/O runs in a background thread and is read off the event
//! loop non-blocking (a `try_recv` on an mpsc channel), so a slow or
//! unresponsive daemon can never freeze the UI: it keeps showing the last
//! good state, stays responsive to keys, and retries on the next tick.

use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use crossterm::event;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;

use crate::config::types::InitialMode;
use crate::curve::CurvePoint;
use crate::error::Error;
use crate::ipc::{Client, Mode, Request, Response, State};

/// A single in-flight request to the daemon. The worker thread delivers its
/// result (or error) on the receiver; the event loop reads it with
/// `try_recv`, so it never has to wait for the daemon.
#[derive(Debug)]
pub enum Job {
    /// A `get-state` poll.
    GetState(Receiver<Result<Response, Error>>),
    /// A `set-mode` command.
    SetMode(Receiver<Result<Response, Error>>),
    /// A `save-fan-config` command (curve + default mode).
    SaveConfig(Receiver<Result<Response, Error>>),
    /// A `sweep` (calibration) command. The daemon runs the sweep in the
    /// background; this job only carries the *start* answer (the result
    /// arrives later in a `get-state` snapshot, shown in the status line).
    Sweep(Receiver<Result<Response, Error>>),
}

/// The "set exact duty" dialog: a modal numeric input (0-100) for the
/// selected fan, opened with `Enter`.
#[derive(Debug, Default)]
pub struct ValueEntry {
    /// The text typed so far (digits only, at most 3 characters).
    pub text: String,
    /// A validation message to display, if the last submit was rejected.
    pub error: Option<String>,
}

/// The "edit curve and default mode" dialog: a modal table editor for the
/// selected fan, opened with `e`. It edits a working copy of the fan's
/// config list; `s` persists it through the daemon (which writes the config
/// file atomically), everything else stays local until saved or cancelled.
#[derive(Debug)]
pub struct CurveEditor {
    /// The points being edited (a working copy of the fan's config list).
    pub points: Vec<CurvePoint>,
    /// The row under the cursor: a point row, or `points.len()` for the
    /// "default mode" row.
    pub row: usize,
    /// The field under the cursor on a point row: 0 = temp, 1 = duty.
    pub field: usize,
    /// The text being typed into the active field.
    pub text: String,
    /// The initial mode being set for the fan.
    pub default_mode: InitialMode,
    /// A validation message, if the last commit was rejected.
    pub error: Option<String>,
    /// A save is in flight: the dialog is locked until the daemon answers.
    pub saving: bool,
}

impl CurveEditor {
    /// Whether the cursor is on the "default mode" row.
    pub fn on_default_row(&self) -> bool {
        self.row >= self.points.len()
    }

    /// Commit the active field of the active point row. Returns `true`
    /// (trivially) when the cursor is on the default row.
    pub fn commit_field(&mut self) -> bool {
        if self.on_default_row() {
            return true;
        }
        let field = self.field;
        let text = self.text.trim();
        let Ok(v) = text.parse::<f64>() else {
            self.error = Some("please enter a number".into());
            return false;
        };
        let (lo, hi) = if field == 0 {
            (0.0, 150.0)
        } else {
            (0.0, 100.0)
        };
        if !(lo..=hi).contains(&v) {
            self.error = Some(format!("must be in {lo}..={hi}"));
            return false;
        }
        let p = &mut self.points[self.row];
        if field == 0 {
            p.temp = v;
        } else {
            p.duty = v;
        }
        self.text = format!("{v}");
        self.error = None;
        true
    }

    /// Move the cursor to `row` (the last index is the default row) and
    /// `field`, refreshing the text buffer with the target's value.
    pub fn move_to(&mut self, row: usize, field: usize) {
        let last = self.points.len();
        let row = row.min(last);
        let field = if row < last { field.min(1) } else { 0 };
        self.row = row;
        self.field = field;
        if row < last {
            let p = &self.points[row];
            self.text = if field == 0 {
                format!("{}", p.temp)
            } else {
                format!("{}", p.duty)
            };
        } else {
            self.text.clear();
        }
        self.error = None;
    }

    /// Add a point after the active row (a copy of it, shifted +10 °C so
    /// the list stays valid), with the cursor on its temp field.
    pub fn add_point(&mut self) {
        let base = self
            .points
            .get(self.row.min(self.points.len() - 1))
            .cloned()
            .unwrap_or_else(|| CurvePoint::new(0.0, 0.0));
        let mut p = base;
        p.temp += 10.0;
        let at = (self.row + 1).min(self.points.len());
        self.points.insert(at, p);
        self.row = at;
        self.field = 0;
        self.text = format!("{}", p.temp);
        self.error = None;
    }

    /// Delete the active point row (at least one point must remain).
    pub fn delete_point(&mut self) {
        if !self.on_default_row() && self.points.len() > 1 {
            self.points.remove(self.row);
            self.row = self.row.min(self.points.len());
            if self.row < self.points.len() {
                self.text = format!("{}", self.points[self.row].temp);
            } else {
                self.text.clear();
            }
            self.field = 0;
            self.error = None;
        }
    }

    /// Move the active point row up (-1) or down (+1).
    pub fn move_row(&mut self, delta: isize) {
        if self.on_default_row() {
            return;
        }
        let n = self.points.len();
        let to = (self.row as isize + delta).clamp(0, n as isize - 1) as usize;
        if to != self.row {
            let p = self.points.remove(self.row);
            self.points.insert(to, p);
            self.row = to;
            self.text = format!("{}", self.points[to].temp);
            self.error = None;
        }
    }

    /// Cycle the default mode on the default row: auto -> off -> full.
    pub fn cycle_default(&mut self, dir: isize) {
        let order = [InitialMode::Auto, InitialMode::Off, InitialMode::Full];
        let i = order
            .iter()
            .position(|m| *m == self.default_mode)
            .unwrap_or(0);
        self.default_mode = order[(i as isize + dir).rem_euclid(order.len() as isize) as usize];
        self.text.clear();
        self.error = None;
    }

    /// Push a typed character into the active field (digits and `.` only,
    /// at most 6 characters).
    pub fn push_char(&mut self, c: char) {
        if !self.on_default_row() && (c.is_ascii_digit() || c == '.') && self.text.len() < 6 {
            self.text.push(c);
            self.error = None;
        }
    }

    /// Remove the last typed character.
    pub fn backspace(&mut self) {
        if !self.on_default_row() {
            self.text.pop();
            self.error = None;
        }
    }

    /// Whole-curve validation for saving: points strictly increasing in
    /// temperature. `None` when the curve is acceptable.
    pub fn validate(&self) -> Option<String> {
        if self.points.is_empty() {
            return Some("the curve needs at least one point".into());
        }
        let mut prev = f64::NEG_INFINITY;
        for p in &self.points {
            if p.temp <= prev {
                return Some(format!(
                    "the points must be strictly increasing in temperature ({:.1} °C)",
                    p.temp
                ));
            }
            prev = p.temp;
        }
        None
    }
}

/// Top-level TUI state.
#[derive(Debug)]
pub struct App {
    pub client: Client,
    /// The last state served by the daemon (or `None` until the first
    /// successful fetch, or while the daemon is unreachable).
    pub state: Option<State>,
    pub selected: usize,
    pub sorting: bool,
    pub scroll: usize,
    pub show_help: bool,
    /// A client-side problem message (daemon unreachable, …).
    pub status: String,
    pub quit: bool,
    /// The open "set exact duty" dialog, if any.
    pub value_entry: Option<ValueEntry>,
    /// The open "edit curve and default mode" dialog, if any.
    pub curve_editor: Option<CurveEditor>,
    /// The in-flight request to the daemon, if any.
    pub job: Option<Job>,
    /// A mode change that was queued because a `get-state` poll was already
    /// in flight when it was requested.
    pub pending_mode: Option<(String, Mode)>,
    /// When the next `get-state` poll is due.
    pub next_refresh: Instant,
    /// The last sweep result seen in a snapshot (to notice a *new* one).
    pub last_sweep_report: Option<crate::sweep::SweepReport>,
    /// When the last *new* sweep result was seen; its note is shown in the
    /// status line for a while after that.
    pub sweep_report_at: Option<Instant>,
}

impl App {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            state: None,
            selected: 0,
            sorting: false,
            scroll: 0,
            show_help: false,
            status: String::new(),
            quit: false,
            value_entry: None,
            curve_editor: None,
            job: None,
            pending_mode: None,
            next_refresh: Instant::now(),
            last_sweep_report: None,
            sweep_report_at: None,
        }
    }

    /// Start a `get-state` poll in the background (no-op when one is
    /// already in flight).
    pub fn start_refresh(&mut self) {
        if self.job.is_some() {
            return;
        }
        let client = self.client.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(client.request(&Request::GetState));
        });
        self.job = Some(Job::GetState(rx));
    }

    fn start_set_mode(&mut self, fan: String, mode: Mode) {
        let client = self.client.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(client.request(&Request::SetMode { fan, mode }));
        });
        self.job = Some(Job::SetMode(rx));
    }

    fn start_save(&mut self, fan: String, curve: Vec<CurvePoint>, default_mode: InitialMode) {
        let client = self.client.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(client.request(&Request::SaveFanConfig {
                fan,
                curve,
                default_mode,
            }));
        });
        self.job = Some(Job::SaveConfig(rx));
    }

    fn start_sweep(&mut self, fan: String) {
        let client = self.client.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(client.request(&Request::Sweep {
                fan,
                step: None,
                settle: None,
            }));
        });
        self.job = Some(Job::Sweep(rx));
    }

    /// Ask the daemon to calibrate the selected fan (key `c`): sweep its
    /// duty 0→100 %, derive the range in which it responds, and save it.
    fn start_calibrate(&mut self) {
        let Some(state) = self.state.as_ref() else {
            self.status = "waiting for the first state from fanctld …".into();
            return;
        };
        let Some(fan) = state.fans.get(self.selected) else {
            return;
        };
        if self.job.is_some() {
            self.status = "fanctld is busy with the previous command".into();
            return;
        }
        if state.sweeping.is_some() {
            self.status = "a calibration sweep is already running".into();
            return;
        }
        self.start_sweep(fan.id.clone());
    }

    /// Collect a job that has *already* completed (non-blocking; a job
    /// still in flight is put back and tried again on the next call).
    pub fn pump(&mut self) {
        let Some(job) = self.job.take() else {
            return;
        };
        let was_set_mode = matches!(job, Job::SetMode(_));
        match job {
            Job::GetState(rx) => match rx.try_recv() {
                Ok(Ok(Response::State { state })) => {
                    // A new sweep result (the daemon's `last_sweep`
                    // changed): start the countdown that shows its note in
                    // the status line.
                    if state.sweep != self.last_sweep_report {
                        self.last_sweep_report = state.sweep.clone();
                        self.sweep_report_at = Some(Instant::now());
                    }
                    self.state = Some(state);
                    self.status.clear();
                    self.next_refresh = Instant::now() + self.refresh_interval();
                }
                Ok(Ok(_)) => {
                    self.status = "fanctld: unexpected response".into();
                    self.next_refresh = Instant::now() + Duration::from_millis(500);
                }
                Ok(Err(e)) => {
                    self.state = None;
                    self.status = format!("can't reach fanctld: {e}");
                    self.next_refresh = Instant::now() + Duration::from_millis(500);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.status = "fanctlui: internal error while polling fanctld".into();
                    self.next_refresh = Instant::now() + Duration::from_millis(500);
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Still in flight: put it back.
                    self.job = Some(Job::GetState(rx));
                    return;
                }
            },
            Job::SetMode(rx) => match rx.try_recv() {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => self.status = format!("can't reach fanctld: {e}"),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.status = "fanctlui: internal error while talking to fanctld".into();
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Still in flight: put it back.
                    self.job = Some(Job::SetMode(rx));
                    return;
                }
            },
            Job::SaveConfig(rx) => match rx.try_recv() {
                Ok(Ok(Response::Ok)) => {
                    let id = self
                        .state
                        .as_ref()
                        .and_then(|s| s.fans.get(self.selected))
                        .map(|f| f.id.clone())
                        .unwrap_or_default();
                    self.status = format!("{id}: saved");
                    self.curve_editor = None;
                    self.next_refresh = Instant::now();
                }
                Ok(Ok(Response::Error { message })) => {
                    // The daemon rejected the curve: keep the dialog open.
                    if let Some(e) = self.curve_editor.as_mut() {
                        e.error = Some(message);
                        e.saving = false;
                    }
                }
                Ok(Ok(_)) => {
                    self.status = "fanctld: unexpected response".into();
                    self.curve_editor = None;
                    self.next_refresh = Instant::now();
                }
                Ok(Err(e)) => {
                    self.status = format!("can't reach fanctld: {e}");
                    self.curve_editor = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.status = "fanctlui: internal error while talking to fanctld".into();
                    self.curve_editor = None;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Still in flight: put it back.
                    self.job = Some(Job::SaveConfig(rx));
                    return;
                }
            },
            Job::Sweep(rx) => match rx.try_recv() {
                Ok(Ok(Response::Ok)) => {
                    // The daemon started the sweep; its progress and the
                    // result come back in the `get-state` snapshots (the
                    // status line shows the sweep while it runs). Re-poll
                    // immediately to pick them up.
                    self.next_refresh = Instant::now();
                }
                Ok(Ok(Response::Error { message })) => {
                    // The daemon refused the sweep (unknown fan, no
                    // tachometer, a sweep already running, …).
                    self.status = message;
                }
                Ok(Ok(_)) => {
                    self.status = "fanctld: unexpected response".into();
                }
                Ok(Err(e)) => self.status = format!("can't reach fanctld: {e}"),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.status = "fanctlui: internal error while talking to fanctld".into();
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Still in flight: put it back.
                    self.job = Some(Job::Sweep(rx));
                    return;
                }
            },
        }
        // The completed request freed the slot. A completed `set-mode`
        // means re-poll now to show the change; a further queued mode
        // change, if any, goes out immediately.
        if was_set_mode {
            self.next_refresh = Instant::now();
        }
        if let Some((fan, mode)) = self.pending_mode.take() {
            self.start_set_mode(fan, mode);
        }
    }

    /// How long to wait between `get-state` polls: the daemon's
    /// `refresh_secs`, clamped, or a 2 s default before the first state.
    pub fn refresh_interval(&self) -> Duration {
        let secs = self
            .state
            .as_ref()
            .map(|s| s.refresh_secs.max(0.2))
            .unwrap_or(2.0);
        Duration::from_secs_f64(secs)
    }

    /// The note of a recently finished calibration sweep (its result is
    /// shown in the status line for 60 s after it first appears).
    pub fn recent_sweep_note<'a>(&self, state: &'a State) -> Option<&'a str> {
        self.sweep_report_at
            .filter(|at| at.elapsed() < Duration::from_secs(60))
            .and_then(|_| state.sweep.as_ref())
            .map(|r| r.note.as_str())
    }

    /// One iteration of the run loop minus the event wait: collect
    /// completed jobs and start the next poll when it's due. Used by
    /// [`App::run`] and by the tests, so both drive the exact same logic.
    pub fn step(&mut self) {
        self.pump();
        if self.job.is_none() && Instant::now() >= self.next_refresh {
            self.start_refresh();
        }
    }

    /// The main loop: keep the event loop responsive no matter what the
    /// daemon is doing, handle keys, and redraw.
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        loop {
            self.step();

            // Wait for keys; a short bound while a job is in flight, so a
            // stuck daemon can never make the UI unresponsive.
            let wait = if self.job.is_some() {
                Duration::from_millis(100)
            } else {
                self.next_refresh.saturating_duration_since(Instant::now())
            };
            if let Ok(true) = event::poll(wait) {
                if let Ok(Event::Key(k)) = event::read() {
                    self.on_key(k);
                }
                while let Ok(true) = event::poll(Duration::ZERO) {
                    if let Ok(Event::Key(k)) = event::read() {
                        self.on_key(k);
                    }
                }
            }

            if self.quit {
                return Ok(());
            }

            terminal.draw(|f| self.draw(f))?;
        }
    }

    // ---- input -------------------------------------------------------------

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.kind != KeyEventKind::Press {
            return;
        }
        // In raw mode Ctrl+C is a key event, not a signal — bind it.
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        // The curve editor is modal: while it is open, everything except
        // Ctrl+C (quit, handled above) goes to it (Esc cancels).
        if self.curve_editor.is_some() {
            self.on_editor_key(k);
            return;
        }
        // The value dialog is modal: while it is open, everything except
        // Ctrl+C (quit, handled above) and Esc (cancel, handled here) goes
        // to it.
        if self.value_entry.is_some() {
            if k.code == KeyCode::Esc {
                self.value_entry = None;
            } else {
                self.on_value_key(k);
            }
            return;
        }
        match k.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => self.quit = true,
            KeyCode::Char('?') => self.show_help = !self.show_help,
            KeyCode::Esc => self.show_help = false,

            // Fan selection.
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Tab => self.select(-1),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::BackTab => self.select(1),

            // Modes for the selected fan.
            KeyCode::Char('a') | KeyCode::Char('A') => self.request_mode(Mode::Auto),
            KeyCode::Char('f') | KeyCode::Char('F') => self.request_mode(Mode::Full),
            KeyCode::Char('o') | KeyCode::Char('O') => self.request_mode(Mode::Off),
            // Open the dialog for setting an exact duty (0-100%).
            KeyCode::Enter => self.open_value_entry(),
            // Open the curve/default-mode editor for the selected fan.
            KeyCode::Char('e') | KeyCode::Char('E') => self.open_curve_editor(),
            // Calibrate the selected fan: sweep its duty 0→100 % through
            // the daemon and measure the range in which it responds.
            KeyCode::Char('c') | KeyCode::Char('C') => self.start_calibrate(),

            // Sorting + scrolling.
            KeyCode::Char('s') | KeyCode::Char('S') => self.sorting = !self.sorting,
            KeyCode::PageDown => self.scroll = (self.scroll + 6).min(10000),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(6),
            KeyCode::End => self.scroll = 10000,
            KeyCode::Home => self.scroll = 0,

            _ => {}
        }
    }

    fn select(&mut self, delta: isize) {
        let n = self.state.as_ref().map(|s| s.fans.len()).unwrap_or(0);
        if n == 0 {
            return;
        }
        let n = n as isize;
        let cur = self.selected as isize;
        let next = if delta >= 0 {
            (cur + delta) % n
        } else {
            (cur + delta).rem_euclid(n)
        };
        self.selected = next as usize;
    }

    /// Send a mode change for the selected fan. If a poll is in flight, the
    /// change is queued and goes out as soon as that poll finishes.
    fn request_mode(&mut self, mode: Mode) {
        let Some(state) = self.state.as_ref() else {
            self.status = "waiting for the first state from fanctld …".into();
            return;
        };
        let Some(fan) = state.fans.get(self.selected) else {
            return;
        };
        let fan = fan.id.clone();
        match &self.job {
            Some(Job::GetState(_)) => self.pending_mode = Some((fan, mode)),
            _ => self.start_set_mode(fan, mode),
        }
    }

    /// Open the "set exact duty" dialog for the selected fan.
    fn open_value_entry(&mut self) {
        if self.state.as_ref().map(|s| s.fans.len()).unwrap_or(0) == 0 {
            self.status = "waiting for the first state from fanctld …".into();
            return;
        }
        self.value_entry = Some(ValueEntry::default());
    }

    /// Handle a keystroke in the open value dialog (digits, backspace, or
    /// Enter to submit).
    fn on_value_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Backspace => {
                if let Some(e) = self.value_entry.as_mut() {
                    e.text.pop();
                    e.error = None;
                }
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if let Some(e) = self.value_entry.as_mut() {
                    if e.text.len() < 3 {
                        e.text.push(c);
                        e.error = None;
                    }
                }
            }
            KeyCode::Enter => {
                // Take the entry out so the arms below may mutate `self`.
                let Some(mut e) = self.value_entry.take() else {
                    return;
                };
                match e.text.parse::<u16>() {
                    Ok(v) if v <= 100 => {
                        let percent = v as u8;
                        self.value_entry = None;
                        self.request_mode(Mode::Manual { percent });
                    }
                    _ => {
                        e.error = Some("please enter a value between 0 and 100".into());
                        self.value_entry = Some(e);
                    }
                }
            }
            // Everything else is ignored while the dialog is open.
            _ => {}
        }
    }

    /// Open the curve/default-mode editor for the selected fan.
    fn open_curve_editor(&mut self) {
        let Some(fan) = self.state.as_ref().and_then(|s| s.fans.get(self.selected)) else {
            self.status = "waiting for the first state from fanctld …".into();
            return;
        };
        let mut points = fan.curve.clone();
        if points.is_empty() {
            // An empty config curve gets a starter row to edit.
            points.push(CurvePoint::new(0.0, 0.0));
        }
        let text = format!("{}", points[0].temp);
        self.curve_editor = Some(CurveEditor {
            points,
            row: 0,
            field: 0,
            text,
            default_mode: fan.default_mode,
            error: None,
            saving: false,
        });
    }

    /// Validate the edited curve and send it to the daemon (which persists
    /// it). If the commit of the active field fails, the dialog stays open
    /// with the error.
    fn editor_save(&mut self, mut ed: CurveEditor) {
        let ok = if !ed.commit_field() {
            false
        } else if let Some(msg) = ed.validate() {
            ed.error = Some(msg);
            false
        } else if self.job.is_some() {
            ed.error = Some("a request is already in flight; try again".into());
            false
        } else {
            true
        };
        if ok {
            let curve = ed.points.clone();
            let mode = ed.default_mode;
            let fan = self
                .state
                .as_ref()
                .and_then(|s| s.fans.get(self.selected))
                .map(|f| f.id.clone())
                .unwrap_or_default();
            ed.saving = true;
            self.curve_editor = Some(ed);
            self.start_save(fan, curve, mode);
        } else {
            self.curve_editor = Some(ed);
        }
    }

    /// Handle a keystroke in the open curve editor.
    fn on_editor_key(&mut self, k: KeyEvent) {
        if k.kind != KeyEventKind::Press {
            return;
        }
        let Some(mut ed) = self.curve_editor.take() else {
            return;
        };
        if ed.saving {
            // The dialog is locked while the daemon processes the save.
            self.curve_editor = Some(ed);
            return;
        }
        match k.code {
            KeyCode::Esc => {
                // Cancel: the working copy is dropped.
                self.curve_editor = None;
                return;
            }
            KeyCode::Enter => {
                ed.commit_field();
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                self.editor_save(ed);
                return;
            }
            KeyCode::Up => {
                if ed.commit_field() {
                    ed.move_to(
                        (ed.row as isize - 1).rem_euclid(ed.points.len() as isize + 1) as usize,
                        ed.field,
                    );
                }
            }
            KeyCode::Down => {
                if ed.commit_field() {
                    ed.move_to((ed.row + 1) % (ed.points.len() + 1), ed.field);
                }
            }
            KeyCode::Left | KeyCode::Right => {
                let dir = if k.code == KeyCode::Right { 1 } else { -1 };
                if ed.commit_field() {
                    if ed.on_default_row() {
                        ed.cycle_default(dir);
                    } else {
                        let f = if ed.field == 0 { 1 } else { 0 };
                        ed.move_to(ed.row, f);
                    }
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                if ed.commit_field() {
                    ed.add_point();
                }
            }
            KeyCode::Char('x') | KeyCode::Char('X') => {
                if ed.commit_field() {
                    ed.delete_point();
                }
            }
            KeyCode::Char('u') | KeyCode::Char('U') => {
                if ed.commit_field() {
                    ed.move_row(-1);
                }
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                if ed.commit_field() {
                    ed.move_row(1);
                }
            }
            KeyCode::Backspace => ed.backspace(),
            KeyCode::Char(c) => ed.push_char(c),
            _ => {}
        }
        self.curve_editor = Some(ed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    /// A minimal fake daemon: answers `get-state` with a canned state (one
    /// fan, one temperature) and `set-mode` with `ok`, logging every request
    /// line it receives.
    fn fake_daemon(socket: &Path, log: Arc<Mutex<Vec<String>>>) -> std::thread::JoinHandle<()> {
        let socket = socket.to_path_buf();
        std::thread::spawn(move || {
            let Ok(listener) = UnixListener::bind(&socket) else {
                return;
            };
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let mut line = String::new();
                let mut reader = std::io::BufReader::new(&mut stream);
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                log.lock().unwrap().push(line.trim().to_string());
                let resp = if line.contains("\"get-state\"") {
                    r#"{"op":"state","state":{"refresh_secs":2.0,"status":"","fans":[{"id":"pwm1","label":"Fan 1","mode":{"kind":"auto"},"duty":70.0,"rpm":1650,"err":null,"curve":[{"temp":30.0,"duty":0.0},{"temp":75.0,"duty":100.0}],"default_mode":"auto"}],"temps":[{"refname":"k10temp:temp1","label":"Tctl","source":"hwmon","celsius":70.0}],"sweeping":null,"sweep":null}}"#
                } else {
                    r#"{"op":"ok"}"#
                };
                let _ = writeln!(stream, "{resp}");
            }
        })
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn digit(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// Drive one run-loop iteration per call until the predicate holds or
    /// the deadline passes.
    fn settle<F>(app: &mut App, deadline_ms: u64, mut cond: F)
    where
        F: FnMut(&App) -> bool,
    {
        let deadline = Instant::now() + Duration::from_millis(deadline_ms);
        while !cond(app) && Instant::now() < deadline {
            app.step();
            std::thread::sleep(Duration::from_millis(10));
        }
        app.step();
    }

    #[test]
    fn q_quits() {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        fake_daemon(&dir.path().join("s.sock"), log);

        let mut app = App::new(Client::new(dir.path().join("s.sock")));
        app.start_refresh();
        settle(&mut app, 5000, |a| a.state.is_some());
        assert_eq!(app.state.as_ref().map(|s| s.fans.len()), Some(1));
        assert!(!app.quit);
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn ctrl_c_quits() {
        let mut app = App::new(Client::new(
            Path::new("/nonexistent/fanctld.sock").to_path_buf(),
        ));
        let k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.on_key(k);
        assert!(app.quit, "Ctrl+C is a key event in raw mode; bind it");
    }

    #[test]
    fn set_mode_key_sends_request_to_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        fake_daemon(&dir.path().join("s.sock"), log.clone());

        let mut app = App::new(Client::new(dir.path().join("s.sock")));
        app.start_refresh();
        settle(&mut app, 5000, |a| a.state.is_some());
        // Manual 50% is set through the Enter dialog: open, type, submit.
        // (The dialog takes the literal value, so 50% is typed as "50".)
        app.on_key(key(KeyCode::Enter));
        app.on_key(digit('5'));
        app.on_key(digit('0'));
        app.on_key(key(KeyCode::Enter));
        assert!(app.value_entry.is_none(), "the dialog must close on submit");

        let mut found = false;
        let deadline = Instant::now() + Duration::from_millis(5000);
        while !found && Instant::now() < deadline {
            app.step();
            let reqs = log.lock().unwrap().clone();
            found = reqs.iter().any(|l| {
                l.contains("\"set-mode\"") && l.contains("\"pwm1\"") && l.contains("\"percent\":50")
            });
            if !found {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(found, "a set-mode request should have been sent");
        app.step();
        assert!(app.status.is_empty(), "no error expected: {}", app.status);
    }

    #[test]
    fn value_dialog_rejects_bad_input_and_esc_cancels() {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        fake_daemon(&dir.path().join("s.sock"), log.clone());

        let mut app = App::new(Client::new(dir.path().join("s.sock")));
        app.start_refresh();
        settle(&mut app, 5000, |a| a.state.is_some());

        app.on_key(key(KeyCode::Enter));
        assert!(app.value_entry.is_some(), "Enter must open the dialog");
        app.on_key(digit('1'));
        app.on_key(digit('0'));
        app.on_key(digit('5'));
        app.on_key(key(KeyCode::Enter));
        // 105 is out of range: the dialog stays open with an error.
        let e = app.value_entry.as_ref().unwrap();
        assert_eq!(e.text, "105");
        assert!(e.error.is_some(), "105 > 100 must be rejected");
        // A fourth digit must not be accepted (3 characters is the cap).
        app.on_key(digit('9'));
        assert_eq!(app.value_entry.as_ref().unwrap().text, "105");

        // Esc cancels the dialog; nothing may have been sent.
        app.on_key(key(KeyCode::Esc));
        assert!(app.value_entry.is_none());
        let reqs = log.lock().unwrap().clone();
        assert!(!reqs.iter().any(|l| l.contains("\"set-mode\"")), "{reqs:?}");
    }

    #[test]
    fn value_dialog_is_modal() {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        fake_daemon(&dir.path().join("s.sock"), log);

        let mut app = App::new(Client::new(dir.path().join("s.sock")));
        app.start_refresh();
        settle(&mut app, 5000, |a| a.state.is_some());

        app.on_key(key(KeyCode::Enter));
        // While typing, ordinary TUI keys must not act.
        app.on_key(key(KeyCode::Char('q')));
        app.on_key(key(KeyCode::Char('a')));
        assert!(!app.quit, "q must not quit while the dialog is open");
        assert!(app.value_entry.is_some(), "the dialog must stay open");
        assert!(app.job.is_none(), "a must not send a mode while typing");
    }

    #[test]
    fn unreachable_daemon_sets_status_and_clears_state() {
        let mut app = App::new(Client::new(
            Path::new("/nonexistent/fanctld.sock").to_path_buf(),
        ));
        app.start_refresh();
        settle(&mut app, 5000, |a| a.status.contains("can't reach fanctld"));
        assert!(app.state.is_none());
    }

    #[test]
    fn e_opens_the_curve_editor() {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        fake_daemon(&dir.path().join("s.sock"), log);

        let mut app = App::new(Client::new(dir.path().join("s.sock")));
        app.start_refresh();
        settle(&mut app, 5000, |a| a.state.is_some());

        app.on_key(key(KeyCode::Char('e')));
        let Some(e) = &app.curve_editor else {
            panic!("e must open the editor");
        };
        assert_eq!(e.points.len(), 2);
        assert_eq!(e.points[0].temp, 30.0);
        assert_eq!(e.points[0].duty, 0.0);
        assert_eq!(e.row, 0);
        assert_eq!(e.field, 0);
        assert_eq!(e.text, "30", "the cursor's field starts prefilled");
    }

    #[test]
    fn editor_edit_and_save_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        fake_daemon(&dir.path().join("s.sock"), log.clone());

        let mut app = App::new(Client::new(dir.path().join("s.sock")));
        app.start_refresh();
        settle(&mut app, 5000, |a| a.state.is_some());

        app.on_key(key(KeyCode::Char('e')));
        // Rewrite the first point's temp: clear "30", type "40", commit.
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Backspace));
        app.on_key(digit('4'));
        app.on_key(digit('0'));
        app.on_key(key(KeyCode::Enter));
        let e = app.curve_editor.as_ref().unwrap();
        assert_eq!(e.points[0].temp, 40.0);

        // Add a point after the cursor.
        app.on_key(key(KeyCode::Char('a')));
        let e = app.curve_editor.as_ref().unwrap();
        assert_eq!(e.points.len(), 3);
        assert_eq!(e.row, 1, "the cursor lands on the new row");
        assert_eq!(e.text, "50");

        // Save.
        app.on_key(key(KeyCode::Char('s')));
        assert!(
            app.curve_editor.as_ref().unwrap().saving,
            "s must lock the dialog while the daemon works"
        );
        settle(&mut app, 5000, |a| a.curve_editor.is_none());
        assert!(
            app.curve_editor.is_none(),
            "the editor must close after a save: {}",
            app.status
        );
        // The status may show "saved" (before the next state arrives) or have
        // been cleared by that state; either way there must be no error.
        assert!(
            app.status.is_empty() || app.status.contains("saved"),
            "unexpected status: {}",
            app.status
        );

        let reqs = log.lock().unwrap().clone();
        assert!(
            reqs.iter().any(|l| l.contains("\"save-fan-config\"")
                && l.contains("\"temp\":40.0")
                && l.contains("\"temp\":75.0")),
            "{reqs:?}"
        );
    }

    #[test]
    fn editor_rejects_bad_values_and_esc_cancels() {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        fake_daemon(&dir.path().join("s.sock"), log.clone());

        let mut app = App::new(Client::new(dir.path().join("s.sock")));
        app.start_refresh();
        settle(&mut app, 5000, |a| a.state.is_some());

        app.on_key(key(KeyCode::Char('e')));
        // Move to the duty field, type 105, and commit: rejected.
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Backspace));
        app.on_key(digit('1'));
        app.on_key(digit('0'));
        app.on_key(digit('5'));
        app.on_key(key(KeyCode::Enter));
        let e = app.curve_editor.as_ref().unwrap();
        assert_eq!(
            e.points[0].duty, 0.0,
            "an out-of-range duty must not be written"
        );
        assert!(
            e.error.as_deref().is_some_and(|m| m.contains("100")),
            "{:?}",
            e.error
        );

        // Esc cancels; nothing must be sent.
        app.on_key(key(KeyCode::Esc));
        assert!(app.curve_editor.is_none());
        let reqs = log.lock().unwrap().clone();
        assert!(
            !reqs.iter().any(|l| l.contains("\"save-fan-config\"")),
            "{reqs:?}"
        );
    }

    #[test]
    fn editor_add_delete_move_and_default_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        fake_daemon(&dir.path().join("s.sock"), log);

        let mut app = App::new(Client::new(dir.path().join("s.sock")));
        app.start_refresh();
        settle(&mut app, 5000, |a| a.state.is_some());

        app.on_key(key(KeyCode::Char('e')));
        // Add a point, move it to the end, delete it.
        app.on_key(key(KeyCode::Char('a')));
        app.on_key(key(KeyCode::Char('d')));
        let e = app.curve_editor.as_ref().unwrap();
        assert_eq!(e.row, 2);
        assert_eq!(
            (e.points[0].temp, e.points[1].temp, e.points[2].temp),
            (30.0, 75.0, 40.0)
        );
        app.on_key(key(KeyCode::Char('x')));
        let e = app.curve_editor.as_ref().unwrap();
        assert_eq!(e.points.len(), 2);
        assert!(e.on_default_row(), "the cursor lands on the default row");

        // The default row is cycled with Left/Right: auto -> full -> off.
        app.on_key(key(KeyCode::Left));
        assert_eq!(
            app.curve_editor.as_ref().unwrap().default_mode,
            crate::config::types::InitialMode::Full
        );
        app.on_key(key(KeyCode::Left));
        assert_eq!(
            app.curve_editor.as_ref().unwrap().default_mode,
            crate::config::types::InitialMode::Off
        );
        app.on_key(key(KeyCode::Esc));
        assert!(app.curve_editor.is_none());
    }

    #[test]
    fn editor_is_modal() {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        fake_daemon(&dir.path().join("s.sock"), log);

        let mut app = App::new(Client::new(dir.path().join("s.sock")));
        app.start_refresh();
        settle(&mut app, 5000, |a| a.state.is_some());

        app.on_key(key(KeyCode::Char('e')));
        // Ordinary TUI keys must not act while the editor is open.
        app.on_key(key(KeyCode::Char('q')));
        app.on_key(key(KeyCode::Char('a')));
        assert!(!app.quit, "q must not quit while the editor is open");
        assert_eq!(
            app.curve_editor.as_ref().unwrap().points.len(),
            3,
            "a adds a point inside the editor"
        );
        assert!(app.job.is_none(), "a must not send a mode request");
        app.on_key(key(KeyCode::Esc));
        assert!(app.curve_editor.is_none());
    }

    #[test]
    fn c_key_sends_a_sweep_request() {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        fake_daemon(&dir.path().join("s.sock"), log.clone());

        let mut app = App::new(Client::new(dir.path().join("s.sock")));
        app.start_refresh();
        settle(&mut app, 5000, |a| a.state.is_some());

        app.on_key(key(KeyCode::Char('c')));
        assert!(matches!(app.job, Some(Job::Sweep(_))));
        settle(&mut app, 5000, |a| a.job.is_none());
        let reqs = log.lock().unwrap().clone();
        assert!(
            reqs.iter().any(|l| l.contains("\"op\":\"sweep\"")),
            "the daemon should have received a sweep request: {reqs:?}"
        );
    }

    #[test]
    fn c_key_is_refused_while_a_sweep_is_running() {
        let mut app = App::new(Client::new(std::path::PathBuf::from(
            "/nonexistent/fanctld.sock",
        )));
        let st = State {
            refresh_secs: 2.0,
            status: String::new(),
            fans: vec![crate::ipc::FanState {
                id: "pwm1".into(),
                label: "Fan 1".into(),
                mode: Mode::Auto,
                duty: Some(30.0),
                rpm: None,
                err: None,
                curve: Vec::new(),
                default_mode: crate::config::types::InitialMode::Auto,
            }],
            temps: Vec::new(),
            sweeping: Some("pwm1".into()),
            sweep: None,
        };
        app.state = Some(st);

        app.on_key(key(KeyCode::Char('c')));
        assert!(app.job.is_none(), "no request while a sweep runs");
        assert!(app.status.contains("already running"), "{}", app.status);
    }

    #[test]
    fn sweep_job_reports_start_and_refusal() {
        let mut app = App::new(Client::new(std::path::PathBuf::from(
            "/nonexistent/fanctld.sock",
        )));

        // The "started" answer: the poll is re-armed immediately (the
        // progress shows up in the next snapshot).
        let (tx, rx) = mpsc::channel();
        app.job = Some(Job::Sweep(rx));
        tx.send(Ok(Response::Ok)).unwrap();
        app.pump();
        assert!(app.job.is_none());
        assert!(app.next_refresh <= Instant::now() + Duration::from_millis(50));

        // A refusal from the daemon lands in the status line.
        let (tx, rx) = mpsc::channel();
        app.job = Some(Job::Sweep(rx));
        tx.send(Ok(Response::Error {
            message: "no fan pwm9 in the config".into(),
        }))
        .unwrap();
        app.pump();
        assert!(app.status.contains("no fan pwm9"), "{}", app.status);
    }
}
