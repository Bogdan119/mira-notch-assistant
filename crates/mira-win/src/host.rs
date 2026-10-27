//! Native Windows host: a per-pixel-alpha layered window drawn with GDI+.
//! No browser runtime; all state lives on the UI thread.
//!
//! The robot has three ways of being:
//! * **docked** — the notch at the top of the primary monitor; hover to open
//!   the tabs (music, focus, water, reminders, shelf, notes, clipboard, folder);
//! * **dragged** — pull it down and it becomes a little box whose eyes keep
//!   watching the cursor; drop it on a folder to load that folder's files;
//! * **pet** — dropped anywhere else it stays out and, every 2–3 s, looks
//!   where the cursor went and hops after it. Double-click sends it home.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mira_core::{
    pomodoro::PomodoroEngine,
    reminder::{self, Reminder, Urgency},
    store::{self, Config, Store},
    tokens::Tokens,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Gdi::*, GdiPlus::GpBitmap},
    System::{
        DataExchange::{AddClipboardFormatListener, GetClipboardSequenceNumber},
        LibraryLoader::GetModuleHandleW,
        Ole::OleInitialize,
        SystemInformation::GetLocalTime,
    },
    UI::{
        Controls::EM_SETLIMITTEXT, HiDpi::*, Input::KeyboardAndMouse::*, Shell::ShellExecuteW,
        WindowsAndMessaging::*,
    },
};

use crate::anim::{bounce_in, ease_out, Pet, Spring};
use crate::com::{self, Clip, Target};
use crate::files::{self, FolderContext, Search};
use crate::gfx::{self, fade, hex, Align, Argb, Canvas, Face, R};
use crate::wide;

const OPEN_W: f32 = 640.;
const OPEN_H: f32 = 272.;
const HEADER_H: f32 = 48.;
const BOX_W: f32 = 124.;
const BOX_H: f32 = 78.;
/// Pet window: the box plus room for squash, stretch and the hop.
const PET_WIN_W: f32 = 220.;
const PET_WIN_H: f32 = 200.;

const WM_OPEN_EDITOR: u32 = WM_APP + 2;
const WM_CONTEXT_READY: u32 = WM_APP + 20;
const WM_SEARCH_PROGRESS: u32 = WM_APP + 21;

// Segoe Fluent Icons / MDL2 Assets code points.
const I_MUSIC: char = '\u{E8D6}';
const I_TIMER: char = '\u{E916}';
const I_BELL: char = '\u{EA8F}';
const I_SHELF: char = '\u{E8F1}';
const I_NOTE: char = '\u{E70B}';
const I_CLIP: char = '\u{E77F}';
const I_FOLDER: char = '\u{E8B7}';
const I_SEARCH: char = '\u{E721}';
const I_PLAY: char = '\u{E768}';
const I_PAUSE: char = '\u{E769}';
const I_PREV: char = '\u{E892}';
const I_NEXT: char = '\u{E893}';
const I_ADD: char = '\u{E710}';
const I_REMOVE: char = '\u{E738}';
const I_DELETE: char = '\u{E74D}';
const I_CLOSE: char = '\u{E711}';
const I_CHECK: char = '\u{E73E}';
const I_COPY: char = '\u{E8C8}';
const I_RESET: char = '\u{E72C}';
const I_SKIP: char = '\u{E893}';
const I_LEFT: char = '\u{E76B}';
const I_RIGHT: char = '\u{E76C}';
const I_SNOOZE: char = '\u{E823}';
const I_DOC: char = '\u{E8A5}';

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Music,
    Focus,
    Water,
    Reminders,
    Shelf,
    Notes,
    Clipboard,
    Folder,
}

const PAGES: [Page; 8] = [
    Page::Music,
    Page::Focus,
    Page::Water,
    Page::Reminders,
    Page::Shelf,
    Page::Notes,
    Page::Clipboard,
    Page::Folder,
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Docked,
    Dragging,
    Pet,
    Returning,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Note(usize),
    Search,
}

#[derive(Clone, Debug, PartialEq)]
enum Action {
    Tab(Page),
    MediaPrev,
    MediaPlay,
    MediaNext,
    FocusToggle,
    FocusSkip,
    FocusReset,
    Water(i64),
    RemPrev,
    RemNext,
    RemNew,
    RemSnooze,
    RemDone,
    ShelfOpen(usize),
    ShelfRemove(usize),
    NoteNew,
    NoteEdit(usize),
    NoteCopy(usize),
    NoteDelete(usize),
    ClipCopy(usize),
    ClipDelete(usize),
    ClipClear,
    FileOpen(PathBuf, usize),
    SearchFocus,
    SearchRun,
    FolderClear,
}

/// What the window procedure must do once the host borrow is released
/// (modal loops re-enter the window procedure).
enum After {
    Nothing,
    Menu,
    DragOut(PathBuf),
}

struct ShelfEntry {
    id: i64,
    path: PathBuf,
    added: Instant,
}

struct ClipEntry {
    clip: Clip,
    at: SystemTime,
}

struct Press {
    screen: POINT,
    action: Option<Action>,
    drag_path: Option<PathBuf>,
    moved: bool,
}

struct Host {
    tokens: Tokens,
    config: Config,
    store: Store,
    focus: PomodoroEngine,
    canvas: Canvas,
    hwnd: HWND,
    scale: f32,

    mode: Mode,
    open: bool,
    open_anim: Spring,
    page: Page,
    away: Option<Instant>,
    peek_until: Option<Instant>,
    file_drag: bool,

    gaze: (f32, f32),
    blink: Instant,
    closed_blink: bool,
    pulse: Option<Instant>,
    pet: Pet,
    next_look: Instant,

    last_tick: Instant,
    last_second: Instant,
    clock: String,
    date: String,
    day: String,
    water: u32,
    reminders: Vec<Reminder>,
    selected_reminder: usize,

    shelf: Vec<ShelfEntry>,
    notes: Vec<String>,
    clips: Vec<ClipEntry>,
    own_clip_seq: u32,
    clip_retry: u8,

    edit: Option<Field>,
    query: String,
    context: Option<FolderContext>,
    context_loading: Option<PathBuf>,
    pending_context: Arc<Mutex<Option<FolderContext>>>,
    search: Option<Search>,
    scroll: HashMap<u8, f32>,
    scroll_max: f32,

    icons: HashMap<(PathBuf, bool), *mut GpBitmap>,
    hits: Vec<(R, Action)>,
    press: Option<Press>,
    mouse: Option<(f32, f32)>,
    toast: Option<(String, Instant)>,
    error: Option<String>,
    /// Content opacity for the current frame (fades with the open spring).
    alpha: f32,
    clock_t: f32,
    dirty: bool,
    /// Horizontal offset of the open layout inside the (animating) notch.
    content_dx: f32,
}

thread_local! { static HOST: RefCell<Option<Host>> = const { RefCell::new(None) }; }

/// Run `f` on the host unless it is already borrowed (re-entrant message).
fn with_host<T>(f: impl FnOnce(&mut Host) -> T) -> Option<T> {
    HOST.with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        guard.as_mut().map(f)
    })
}

fn epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string_lossy().to_string())
}

impl Host {
    unsafe fn new(hwnd: HWND) -> Result<Self, Box<dyn std::error::Error>> {
        let config = store::config_path().and_then(|p| Config::load_or_default(&p))?;
        let db = Store::open(&store::db_path()?)?;
        let reminders = db.reminders()?;
        let shelf = db
            .shelf_items()?
            .into_iter()
            .map(|i| ShelfEntry {
                id: i.id,
                path: PathBuf::from(i.path),
                // Already-shelved items don't replay the drop-in bounce.
                added: Instant::now() - Duration::from_secs(10),
            })
            .collect();
        let canvas = Canvas::new().ok_or("GDI+ could not start")?;
        let now = Instant::now();
        Ok(Host {
            tokens: Tokens::embedded()?,
            focus: PomodoroEngine::new(config.pomodoro_config()),
            config,
            store: db,
            canvas,
            hwnd,
            scale: 1.,
            mode: Mode::Docked,
            open: false,
            open_anim: Spring::new(0., 420., 30.),
            page: Page::Music,
            away: None,
            peek_until: None,
            file_drag: false,
            gaze: (0., 0.),
            blink: now,
            closed_blink: false,
            pulse: None,
            pet: Pet::new(0., 0.),
            next_look: now,
            last_tick: now,
            last_second: now - Duration::from_secs(1),
            clock: String::new(),
            date: String::new(),
            day: String::new(),
            water: 0,
            reminders,
            selected_reminder: 0,
            shelf,
            notes: Vec::new(),
            clips: Vec::new(),
            own_clip_seq: 0,
            clip_retry: 0,
            edit: None,
            query: String::new(),
            context: None,
            context_loading: None,
            pending_context: Arc::new(Mutex::new(None)),
            search: None,
            scroll: HashMap::new(),
            scroll_max: 0.,
            icons: HashMap::new(),
            hits: Vec::new(),
            press: None,
            mouse: None,
            toast: None,
            error: None,
            alpha: 1.,
            clock_t: 0.,
            dirty: true,
            content_dx: 0.,
        })
    }

    fn ro(&self) -> bool {
        matches!(self.config.language, store::Language::Ro)
    }

    fn tr<'a>(&self, ro: &'a str, en: &'a str) -> &'a str {
        if self.ro() {
            ro
        } else {
            en
        }
    }

    fn color(&self, token: &str) -> Argb {
        let c = &self.tokens.colors;
        hex(match token {
            "bg" => &c.background,
            "text" => &c.text,
            "dim" => &c.text_dim,
            "accent" => &c.accent,
            "sep" => &c.separator,
            "ok" => &c.battery_ok,
            "low" => &c.battery_low,
            _ => &c.text,
        })
    }

    /// Colour with the current content fade applied.
    fn c(&self, argb: Argb) -> Argb {
        fade(argb, self.alpha)
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
        self.dirty = true;
    }

    // --- geometry -------------------------------------------------------------

    unsafe fn monitor(&self) -> RECT {
        let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut info);
        info.rcMonitor
    }

    unsafe fn work_area(&self) -> RECT {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut info);
        info.rcWork
    }

    fn closed_size(&self) -> (f32, f32) {
        let c = self.tokens.notch.closed;
        let w = if self.reminders.is_empty() {
            c.width
        } else {
            300.
        };
        (w, c.height)
    }

    /// Current notch size in logical px (follows the open spring).
    fn notch_size(&self) -> (f32, f32, f32) {
        let (cw, ch) = self.closed_size();
        let t = self.open_anim.value.max(0.);
        (
            lerp(cw, OPEN_W, t),
            lerp(ch, OPEN_H, t),
            lerp(self.tokens.notch.closed.radius_bottom, 30., t.min(1.)),
        )
    }

    /// Screen rect of the window for the current mode, in device px.
    unsafe fn window_rect(&self) -> (i32, i32, i32, i32) {
        match self.mode {
            Mode::Docked => {
                let m = self.monitor();
                let (w, h, _) = self.notch_size();
                let (w, h) = (
                    (w * self.scale).ceil() as i32,
                    (h * self.scale).ceil() as i32,
                );
                (m.left + (m.right - m.left - w) / 2, m.top, w, h)
            }
            _ => {
                let (w, h) = (
                    (PET_WIN_W * self.scale) as i32,
                    (PET_WIN_H * self.scale) as i32,
                );
                (
                    self.pet.x.value as i32 - w / 2,
                    self.pet.y.value as i32 - h / 2,
                    w,
                    h,
                )
            }
        }
    }

    /// Where the robot docks (centre of the closed notch), in device px.
    unsafe fn home(&self) -> (f32, f32) {
        let m = self.monitor();
        let (_, ch) = self.closed_size();
        (
            (m.left + m.right) as f32 / 2.,
            m.top as f32 + ch * self.scale / 2.,
        )
    }

    // --- per-frame update -------------------------------------------------------

    unsafe fn tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        self.clock_t += dt;
        self.scale = GetDpiForWindow(self.hwnd).max(96) as f32 / 96.;

        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        let (wx, wy, ww, wh) = self.window_rect();
        let inside = cursor.x >= wx && cursor.x < wx + ww && cursor.y >= wy && cursor.y < wy + wh;
        let local = self.to_content((
            (cursor.x - wx) as f32 / self.scale,
            (cursor.y - wy) as f32 / self.scale,
        ));
        let mouse = inside.then_some(local);
        if mouse != self.mouse {
            self.mouse = mouse;
            self.dirty |= self.open;
        }

        match self.mode {
            Mode::Docked => self.tick_docked(now, dt, inside),
            Mode::Dragging => {
                self.pet
                    .hold(cursor.x as f32, cursor.y as f32 + 12. * self.scale);
                self.pet.step(dt);
                self.dirty = true;
            }
            Mode::Pet => {
                if now >= self.next_look {
                    self.look_and_follow(cursor);
                }
                if self.pet.busy() {
                    self.pet.step(dt);
                    self.dirty = true;
                }
            }
            Mode::Returning => {
                let (hx, hy) = self.home();
                self.pet.go(hx, hy);
                self.pet.step(dt);
                self.dirty = true;
                if (self.pet.x.value - hx).hypot(self.pet.y.value - hy) < 6. * self.scale {
                    self.mode = Mode::Docked;
                    self.open_anim.snap(0.);
                    if self.peek_until.is_some() {
                        self.set_open(true);
                    }
                }
            }
        }

        self.update_gaze(cursor);
        self.update_clock(now);

        if self.clip_retry > 0 {
            self.clip_retry -= 1;
            self.capture_clipboard();
        }
        let loaded = self.pending_context.lock().unwrap().take();
        if let Some(ctx) = loaded {
            self.context_loaded(ctx);
        }

        // Continuous animations.
        if self.open && self.mode == Mode::Docked {
            let animated = match self.page {
                Page::Shelf => !self.shelf.is_empty() || self.file_drag,
                Page::Notes | Page::Folder => self.edit.is_some(),
                _ => false,
            };
            self.dirty |= animated;
            if self
                .search
                .as_ref()
                .is_some_and(|s| !s.done.load(Ordering::Relaxed))
            {
                self.dirty = true;
            }
        }
        if let Some(t) = self.toast.as_ref().map(|t| t.1) {
            if now.duration_since(t) > Duration::from_millis(2600) {
                self.toast = None;
                self.dirty = true;
            }
        }
        if self
            .pulse
            .is_some_and(|p| now.duration_since(p) < Duration::from_millis(700))
        {
            self.dirty = true;
        }

        if self.dirty {
            self.dirty = false;
            self.render();
        }
    }

    /// Window-local logical point → open-content coordinates (the content is
    /// laid out at full width and centred while the notch grows).
    fn to_content(&self, p: (f32, f32)) -> (f32, f32) {
        (p.0 - self.content_dx, p.1)
    }

    unsafe fn tick_docked(&mut self, now: Instant, dt: f32, inside: bool) {
        let keep_open = self.edit.is_some()
            || self.file_drag
            || self.press.is_some()
            || self.peek_until.is_some_and(|t| now < t);
        if self.peek_until.is_some_and(|t| now >= t) {
            self.peek_until = None;
        }
        if inside {
            self.away = None;
            if !self.open {
                self.set_open(true);
            }
        } else if self.open && !keep_open {
            let since = *self.away.get_or_insert(now);
            if now.duration_since(since).as_millis()
                >= self.tokens.notch.hover_close_delay_ms as u128
            {
                self.set_open(false);
            }
        }
        if !self.open_anim.settled() {
            self.open_anim.step(dt.max(1. / 120.));
            self.dirty = true;
        }
        let urgency = self.reminders.first().map(|r| r.urgency(epoch()));
        if matches!(
            urgency,
            Some(Urgency::Soon | Urgency::Urgent | Urgency::Due)
        ) && !self.open
        {
            // Redraw only during the brief agitation beat.
            self.dirty |= self.clock_t % 1. < 0.5;
        }
    }

    fn set_open(&mut self, open: bool) {
        self.open = open;
        self.away = None;
        self.open_anim.target = if open { 1. } else { 0. };
        if !open {
            self.press = None;
        }
        self.dirty = true;
    }

    /// Every 2–3 s the pet checks where the cursor went and hops after it,
    /// stopping a little short so it sits beside the pointer, not under it.
    unsafe fn look_and_follow(&mut self, cursor: POINT) {
        let jitter = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_millis())
            .unwrap_or(0)
            % 1000) as u64;
        self.next_look = Instant::now() + Duration::from_millis(2000 + jitter);
        let (px, py) = (self.pet.x.value, self.pet.y.value);
        let (cx, cy) = (cursor.x as f32, cursor.y as f32);
        let dist = (px - cx).hypot(py - cy);
        let keep = 110. * self.scale;
        if dist < keep * 1.2 {
            // Close enough: a little happy bounce instead of moving.
            self.pet.squash.velocity -= 5.;
            self.dirty = true;
            return;
        }
        let k = keep / dist;
        let (mut tx, mut ty) = (cx + (px - cx) * k, cy + (py - cy) * k);
        let wa = self.work_area();
        let (hw, hh) = (BOX_W * self.scale / 2., BOX_H * self.scale / 2.);
        tx = tx.clamp(wa.left as f32 + hw, wa.right as f32 - hw);
        ty = ty.clamp(wa.top as f32 + hh + 30. * self.scale, wa.bottom as f32 - hh);
        self.pet.go(tx, ty);
        self.dirty = true;
    }

    unsafe fn update_gaze(&mut self, cursor: POINT) {
        let (cx, cy) = match self.mode {
            Mode::Docked => {
                let (x, y, w, h) = self.window_rect();
                (x as f32 + w as f32 / 2., y as f32 + h as f32 / 2.)
            }
            _ => (
                self.pet.x.value,
                self.pet.y.value - self.pet.lift() * self.scale,
            ),
        };
        let dx = (cursor.x as f32 - cx) / self.scale;
        let dy = (cursor.y as f32 - cy) / self.scale;
        let distance = dx.hypot(dy).max(1.);
        let strength = (distance / 400.).min(1.);
        let target = (dx / distance * strength * 8., dy / distance * strength * 5.);
        let next = (
            self.gaze.0 + (target.0 - self.gaze.0) * 0.3,
            self.gaze.1 + (target.1 - self.gaze.1) * 0.3,
        );
        if (next.0 - self.gaze.0).abs() + (next.1 - self.gaze.1).abs() > 0.01 {
            self.gaze = next;
            self.dirty |= self.mode != Mode::Docked || !self.open || self.open_anim.value < 0.99;
        }
        let now = Instant::now();
        let blinking = now.duration_since(self.blink).as_millis() >= 4000;
        if blinking != self.closed_blink {
            self.closed_blink = blinking;
            self.dirty = true;
        }
        if now.duration_since(self.blink).as_millis() >= 4140 {
            self.blink = now;
        }
    }

    unsafe fn update_clock(&mut self, now: Instant) {
        let seconds = now.duration_since(self.last_second).as_secs();
        if seconds == 0 {
            return;
        }
        self.last_second += Duration::from_secs(seconds);
        for _ in 0..seconds {
            if let mira_core::pomodoro::TickResult::PhaseChanged(_) = self.focus.tick() {
                self.pulse = Some(now);
                self.toast(
                    self.tr("Pomodoro: fază nouă", "Pomodoro: next phase")
                        .to_string(),
                );
            }
        }
        let mut t: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut t);
        self.clock = format!("{:02}:{:02}", t.wHour, t.wMinute);
        let days_ro = ["Dum", "Lun", "Mar", "Mie", "Joi", "Vin", "Sâm"];
        let days_en = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let dow = t.wDayOfWeek as usize % 7;
        self.date = format!(
            "{} {}",
            if self.ro() {
                days_ro[dow]
            } else {
                days_en[dow]
            },
            t.wDay
        );
        let day = format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay);
        if day != self.day {
            self.day = day;
            self.water = self.store.water_glasses(&self.day).unwrap_or(0);
        }
        let mut changed = false;
        for r in &mut self.reminders {
            if !r.notified && r.due_epoch <= epoch() {
                match self.store.mark_reminder_notified(r.id) {
                    Ok(()) => {
                        r.notified = true;
                        changed = true;
                    }
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
        }
        if changed {
            self.pulse = Some(now);
            self.page = Page::Reminders;
            self.peek_until = Some(now + Duration::from_secs(6));
        }
        self.dirty |= self.open || !self.reminders.is_empty() || self.focus.is_running();
    }

    // --- data ---------------------------------------------------------------------

    unsafe fn reload_reminders(&mut self) {
        match self.store.reminders() {
            Ok(items) => self.reminders = items,
            Err(e) => self.error = Some(e.to_string()),
        }
        self.selected_reminder = self
            .selected_reminder
            .min(self.reminders.len().saturating_sub(1));
        self.dirty = true;
    }

    fn add_to_shelf(&mut self, paths: &[PathBuf]) -> usize {
        let mut added = 0;
        for p in paths {
            if self.shelf.iter().any(|e| &e.path == p) {
                continue;
            }
            match self.store.add_shelf_item(&p.to_string_lossy(), epoch()) {
                Ok(id) => {
                    self.shelf.push(ShelfEntry {
                        id,
                        path: p.clone(),
                        added: Instant::now(),
                    });
                    added += 1;
                }
                Err(e) => self.error = Some(e.to_string()),
            }
        }
        self.dirty = true;
        added
    }

    unsafe fn files_dropped(&mut self, paths: Vec<PathBuf>) {
        self.file_drag = false;
        if paths.is_empty() {
            return;
        }
        // A single folder dropped on the Folder tab becomes the search context.
        if self.page == Page::Folder && paths.len() == 1 && paths[0].is_dir() {
            self.load_context(paths[0].clone());
            return;
        }
        let n = self.add_to_shelf(&paths);
        self.page = Page::Shelf;
        self.pet.squash.velocity -= 8.;
        self.pulse = Some(Instant::now());
        if self.mode == Mode::Docked {
            self.set_open(true);
            self.peek_until = Some(Instant::now() + Duration::from_millis(1500));
        }
        let msg = if self.ro() {
            format!("{n} pe raft")
        } else {
            format!("{n} on the shelf")
        };
        self.toast(msg);
    }

    unsafe fn load_context(&mut self, dir: PathBuf) {
        self.context_loading = Some(dir.clone());
        self.search = None;
        self.page = Page::Folder;
        self.scroll.remove(&(Page::Folder as u8));
        let slot = self.pending_context.clone();
        let hwnd = self.hwnd as isize;
        std::thread::spawn(move || {
            let ctx = files::walk(&dir, 5000);
            *slot.lock().unwrap() = Some(ctx);
            PostMessageW(hwnd as HWND, WM_CONTEXT_READY, 0, 0);
        });
        self.dirty = true;
    }

    fn context_loaded(&mut self, ctx: FolderContext) {
        self.context_loading = None;
        let msg = if self.ro() {
            format!(
                "{} · {} fișiere{}",
                file_name(&ctx.root),
                ctx.files.len(),
                if ctx.truncated { "+" } else { "" }
            )
        } else {
            format!(
                "{} · {} files{}",
                file_name(&ctx.root),
                ctx.files.len(),
                if ctx.truncated { "+" } else { "" }
            )
        };
        self.context = Some(ctx);
        self.toast(msg);
        if !self.query.is_empty() {
            self.run_search();
        }
    }

    fn run_search(&mut self) {
        if self.query.trim().is_empty() {
            self.search = None;
            self.dirty = true;
            return;
        }
        let Some(root) = self.context.as_ref().map(|c| c.root.clone()) else {
            self.toast(
                self.tr("Încarcă întâi un folder", "Load a folder first")
                    .to_string(),
            );
            return;
        };
        let hwnd = self.hwnd as isize;
        self.search = Some(files::search(&root, self.query.trim(), move || unsafe {
            PostMessageW(hwnd as HWND, WM_SEARCH_PROGRESS, 0, 0);
        }));
        self.scroll.remove(&(Page::Folder as u8));
        self.dirty = true;
    }

    unsafe fn capture_clipboard(&mut self) {
        let seq = GetClipboardSequenceNumber();
        if seq == self.own_clip_seq {
            self.clip_retry = 0;
            return;
        }
        let Some(clip) = com::read_clipboard(self.hwnd) else {
            return; // busy; retried from tick
        };
        self.clip_retry = 0;
        let same = |a: &Clip, b: &Clip| match (a, b) {
            (Clip::Text(x), Clip::Text(y)) => x == y,
            (Clip::Files(x), Clip::Files(y)) => x == y,
            _ => false,
        };
        let clip = match clip {
            Clip::Text(t) if t.trim().is_empty() => return,
            Clip::Text(t) => Clip::Text(t.chars().take(20_000).collect()),
            other => other,
        };
        self.clips.retain(|c| !same(&c.clip, &clip));
        self.clips.insert(
            0,
            ClipEntry {
                clip,
                at: SystemTime::now(),
            },
        );
        self.clips.truncate(60);
        self.pulse = Some(Instant::now());
        self.dirty = true;
    }

    unsafe fn copy_entry(&mut self, i: usize) {
        let Some(entry) = self.clips.get(i) else {
            return;
        };
        self.own_clip_seq = match &entry.clip {
            Clip::Text(t) => com::write_clipboard_text(self.hwnd, t),
            Clip::Files(f) => com::write_clipboard_files(self.hwnd, f),
        };
        // Move it to the top, like a fresh copy.
        let e = self.clips.remove(i);
        self.clips.insert(0, e);
        self.toast(
            self.tr("Copiat în clipboard", "Copied to clipboard")
                .to_string(),
        );
    }

    unsafe fn paste_into_note(&mut self, i: usize) {
        match com::read_clipboard(self.hwnd) {
            Some(Clip::Text(t)) => {
                if let Some(n) = self.notes.get_mut(i) {
                    n.push_str(&t);
                }
            }
            Some(Clip::Files(f)) => {
                if let Some(n) = self.notes.get_mut(i) {
                    for p in f {
                        if !n.is_empty() && !n.ends_with('\n') {
                            n.push('\n');
                        }
                        n.push_str(&p.to_string_lossy());
                    }
                }
            }
            None => {}
        }
        self.dirty = true;
    }

    // --- text editing ---------------------------------------------------------------

    unsafe fn begin_edit(&mut self, field: Field) {
        self.edit = Some(field);
        let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex & !(WS_EX_NOACTIVATE as isize));
        SetForegroundWindow(self.hwnd);
        SetFocus(self.hwnd);
        self.dirty = true;
    }

    unsafe fn end_edit(&mut self) {
        if self.edit.take().is_some() {
            let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE as isize);
            // Empty notes don't stick around.
            self.notes.retain(|n| !n.trim().is_empty());
            self.dirty = true;
        }
    }

    unsafe fn key_char(&mut self, ch: u16) {
        let Some(field) = self.edit else {
            return;
        };
        let text = match field {
            Field::Note(i) => match self.notes.get_mut(i) {
                Some(t) => t,
                None => return,
            },
            Field::Search => &mut self.query,
        };
        match ch {
            8 => {
                text.pop();
            }
            13 => {
                if field == Field::Search {
                    self.run_search();
                } else {
                    text.push('\n');
                }
            }
            c if c >= 32 => {
                if let Some(c) = char::from_u32(c as u32) {
                    text.push(c);
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    unsafe fn key_down(&mut self, vk: u16) -> bool {
        if self.edit.is_none() {
            return false;
        }
        let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
        if vk == VK_ESCAPE {
            self.end_edit();
            return true;
        }
        if ctrl && vk == b'V' as u16 {
            match self.edit {
                Some(Field::Note(i)) => self.paste_into_note(i),
                Some(Field::Search) => {
                    if let Some(Clip::Text(t)) = com::read_clipboard(self.hwnd) {
                        self.query.push_str(t.lines().next().unwrap_or(""));
                        self.dirty = true;
                    }
                }
                None => {}
            }
            return true;
        }
        if ctrl && vk == b'C' as u16 {
            if let Some(Field::Note(i)) = self.edit {
                if let Some(t) = self.notes.get(i).cloned() {
                    com::write_clipboard_text(self.hwnd, &t);
                    self.toast(self.tr("Notiță copiată", "Note copied").to_string());
                }
            }
            return true;
        }
        false
    }

    // --- mouse -------------------------------------------------------------------------

    fn action_at(&self, p: (f32, f32)) -> Option<Action> {
        self.hits
            .iter()
            .rev()
            .find(|(r, _)| r.contains(p))
            .map(|(_, a)| a.clone())
    }

    unsafe fn mouse_down(&mut self, screen: POINT, local: (f32, f32)) {
        let local = self.to_content(local);
        let action = if self.mode == Mode::Docked && self.open {
            self.action_at(local)
        } else {
            None
        };
        let drag_path = match &action {
            Some(Action::ShelfOpen(i)) => self.shelf.get(*i).map(|e| e.path.clone()),
            Some(Action::FileOpen(p, _)) => Some(p.clone()),
            _ => None,
        };
        self.press = Some(Press {
            screen,
            action,
            drag_path,
            moved: false,
        });
    }

    unsafe fn mouse_move(&mut self, screen: POINT) -> After {
        let Some(press) = self.press.as_mut() else {
            return After::Nothing;
        };
        if press.moved || self.mode == Mode::Dragging {
            return After::Nothing;
        }
        let dx = (screen.x - press.screen.x) as f32;
        let dy = (screen.y - press.screen.y) as f32;
        if dx.hypot(dy) < 8. * self.scale {
            return After::Nothing;
        }
        if let Some(path) = press.drag_path.take() {
            self.press = None;
            return After::DragOut(path);
        }
        if press.action.is_some() {
            // Moved off a button: cancel the click.
            press.moved = true;
            return After::Nothing;
        }
        // Pulling the robot itself (not a button) down detaches it; a sideways
        // wiggle on the docked notch doesn't.
        if dy > 10. * self.scale || self.mode == Mode::Pet {
            press.moved = true;
            self.start_drag(screen);
        }
        After::Nothing
    }

    unsafe fn start_drag(&mut self, screen: POINT) {
        self.end_edit();
        if self.mode == Mode::Docked {
            let (x, y, w, h) = self.window_rect();
            self.pet = Pet::new((x + w / 2) as f32, (y + h / 2) as f32);
        }
        self.mode = Mode::Dragging;
        self.open = false;
        self.open_anim.snap(0.);
        self.pet.hold(screen.x as f32, screen.y as f32);
        self.dirty = true;
    }

    unsafe fn mouse_up(&mut self, screen: POINT) {
        let press = self.press.take();
        if self.mode == Mode::Dragging {
            self.finish_drag(screen);
            return;
        }
        let Some(press) = press else {
            return;
        };
        if press.moved {
            return;
        }
        if let Some(a) = press.action {
            self.perform(a);
        } else if self.mode == Mode::Docked && self.edit.is_some() {
            self.end_edit();
        }
    }

    unsafe fn finish_drag(&mut self, screen: POINT) {
        // Look *through* our own window for what it was dropped on.
        let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_TRANSPARENT as isize);
        let target = com::target_at(screen);
        SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex);

        let near_top = screen.y - self.monitor().top < (60. * self.scale) as i32;
        match target {
            Some(Target::Folder(dir)) => {
                self.load_context(dir);
                self.peek_until = Some(Instant::now() + Duration::from_secs(5));
                self.mode = Mode::Returning;
            }
            Some(Target::File(file)) => {
                self.add_to_shelf(&[file]);
                self.page = Page::Shelf;
                self.peek_until = Some(Instant::now() + Duration::from_secs(3));
                self.mode = Mode::Returning;
            }
            None if near_top => self.mode = Mode::Returning,
            None => {
                self.mode = Mode::Pet;
                self.pet.squash.velocity -= 9.;
                self.next_look = Instant::now() + Duration::from_millis(1500);
            }
        }
        self.dirty = true;
    }

    unsafe fn right_click(&mut self, local: (f32, f32)) -> After {
        let local = self.to_content(local);
        if self.mode == Mode::Docked && self.open {
            match self.action_at(local) {
                Some(Action::ShelfOpen(i)) => {
                    self.perform(Action::ShelfRemove(i));
                    return After::Nothing;
                }
                Some(Action::ClipCopy(i)) => {
                    self.perform(Action::ClipDelete(i));
                    return After::Nothing;
                }
                Some(Action::NoteEdit(i)) => {
                    self.perform(Action::NoteDelete(i));
                    return After::Nothing;
                }
                Some(Action::FileOpen(p, _)) => {
                    self.add_to_shelf(&[p]);
                    self.toast(self.tr("Adăugat pe raft", "Added to the shelf").to_string());
                    return After::Nothing;
                }
                _ => {}
            }
        }
        After::Menu
    }

    unsafe fn wheel(&mut self, delta: i16) {
        let key = self.page as u8;
        let s = self.scroll.entry(key).or_insert(0.);
        *s = (*s - delta as f32 / 120. * 48.).clamp(0., self.scroll_max.max(0.));
        self.dirty = true;
    }

    unsafe fn perform(&mut self, action: Action) {
        self.error = None;
        if !matches!(
            action,
            Action::NoteEdit(_) | Action::NoteNew | Action::SearchFocus
        ) {
            self.end_edit();
        }
        let w = |vk: VIRTUAL_KEY| {
            keybd_event(vk as u8, 0, 0, 0);
            keybd_event(vk as u8, 0, KEYEVENTF_KEYUP, 0);
        };
        match action {
            Action::Tab(p) => {
                self.page = p;
            }
            Action::MediaPrev => w(VK_MEDIA_PREV_TRACK),
            Action::MediaPlay => w(VK_MEDIA_PLAY_PAUSE),
            Action::MediaNext => w(VK_MEDIA_NEXT_TRACK),
            Action::FocusToggle => self.focus.toggle(),
            Action::FocusSkip => {
                self.focus.skip();
            }
            Action::FocusReset => self.focus.reset(),
            Action::Water(d) => match self.store.water_adjust(&self.day, d) {
                Ok(v) => self.water = v,
                Err(e) => self.error = Some(e.to_string()),
            },
            Action::RemPrev => self.selected_reminder = self.selected_reminder.saturating_sub(1),
            Action::RemNext => {
                self.selected_reminder =
                    (self.selected_reminder + 1).min(self.reminders.len().saturating_sub(1))
            }
            Action::RemNew => {
                PostMessageW(self.hwnd, WM_OPEN_EDITOR, 0, 0);
            }
            Action::RemSnooze | Action::RemDone => {
                if let Some(r) = self.reminders.get(self.selected_reminder) {
                    let result = if action == Action::RemSnooze {
                        self.store.snooze_reminder(r.id, epoch() + 300, epoch())
                    } else {
                        self.store.remove_reminder(r.id)
                    };
                    if let Err(e) = result {
                        self.error = Some(e.to_string());
                    }
                    self.reload_reminders();
                }
            }
            Action::ShelfOpen(i) => {
                if let Some(e) = self.shelf.get(i) {
                    open_path(&e.path);
                }
            }
            Action::ShelfRemove(i) => {
                if i < self.shelf.len() {
                    let e = self.shelf.remove(i);
                    if let Err(err) = self.store.remove_shelf_item(e.id) {
                        self.error = Some(err.to_string());
                    }
                }
            }
            Action::NoteNew => {
                self.notes.insert(0, String::new());
                self.scroll.remove(&(Page::Notes as u8));
                self.begin_edit(Field::Note(0));
            }
            Action::NoteEdit(i) => self.begin_edit(Field::Note(i)),
            Action::NoteCopy(i) => {
                if let Some(t) = self.notes.get(i).cloned() {
                    self.own_clip_seq = com::write_clipboard_text(self.hwnd, &t);
                    self.toast(self.tr("Notiță copiată", "Note copied").to_string());
                }
            }
            Action::NoteDelete(i) => {
                if i < self.notes.len() {
                    self.notes.remove(i);
                }
            }
            Action::ClipCopy(i) => self.copy_entry(i),
            Action::ClipDelete(i) => {
                if i < self.clips.len() {
                    self.clips.remove(i);
                }
            }
            Action::ClipClear => self.clips.clear(),
            Action::FileOpen(p, line) => open_at_line(&p, line),
            Action::SearchFocus => self.begin_edit(Field::Search),
            Action::SearchRun => self.run_search(),
            Action::FolderClear => {
                self.context = None;
                self.search = None;
                self.query.clear();
            }
        }
        self.dirty = true;
    }

    // --- drawing ------------------------------------------------------------------------

    unsafe fn render(&mut self) {
        let (x, y, w, h) = self.window_rect();
        self.hits.clear();
        self.canvas.begin(w, h, self.scale);
        match self.mode {
            Mode::Docked => self.draw_docked(),
            _ => self.draw_pet(),
        }
        self.canvas.present(self.hwnd, (x, y), (w, h));
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn eyes(
        &mut self,
        cx: f32,
        cy: f32,
        gap: f32,
        rx: f32,
        ry: f32,
        color: Argb,
        beat: f32,
    ) {
        let pulse = self
            .pulse
            .map(|p| p.elapsed().as_secs_f32())
            .filter(|t| *t < 0.7)
            .map(|t| 1. + (t * 18.).sin() * 0.25 * (1. - t / 0.7))
            .unwrap_or(1.);
        let h = if self.closed_blink {
            ry * 0.16
        } else {
            ry * pulse
        };
        for side in [-1., 1.] {
            let ex = cx + side * gap + self.gaze.0 + beat;
            let ey = cy + self.gaze.1 * 0.8 - beat.abs() * 0.5;
            self.canvas.ellipse(ex, ey, rx * pulse, h, color);
        }
    }

    unsafe fn draw_docked(&mut self) {
        let (w, h, radius) = self.notch_size();
        let t = self.open_anim.value.max(0.);
        let bg = self.color("bg");
        self.canvas.notch(R::new(0., 0., w, h), radius, bg);

        // Closed face fades out as the notch opens.
        let closed_alpha = (1. - t * 2.5).clamp(0., 1.);
        if closed_alpha > 0. {
            self.alpha = closed_alpha;
            self.draw_closed(w, h);
        }
        let open_alpha = ease_out((t - 0.45) / 0.55);
        if open_alpha > 0.01 {
            self.alpha = open_alpha;
            // Clip to the notch first (clips are fixed in device space), then
            // centre the full-width layout in the growing shape.
            self.canvas.clip(R::new(0., 0., w, h), 0.);
            self.content_dx = (w - OPEN_W) / 2.;
            self.canvas.push(self.content_dx, 0., 1., 1.);
            self.draw_open();
            self.canvas.reset();
            self.canvas.unclip();
        }
        self.alpha = 1.;
    }

    unsafe fn draw_closed(&mut self, w: f32, h: f32) {
        let next = self.reminders.first().cloned();
        let urgency = next.as_ref().map(|r| r.urgency(epoch()));
        let color = match urgency {
            Some(Urgency::Due | Urgency::Urgent) => 0xffff7878,
            Some(Urgency::Soon) => 0xffffb65c,
            _ => hex(&self.tokens.colors.visualizer_bar),
        };
        let intensity = match urgency {
            Some(Urgency::Due | Urgency::Urgent) => 2.,
            Some(Urgency::Soon) if self.clock_t % 3. < 0.5 => 0.6,
            _ => 0.,
        };
        let beat = if self.clock_t % 1. < 0.45 {
            (self.clock_t * 32.).sin() * intensity
        } else {
            0.
        };
        let cx = if next.is_some() { 40. } else { w / 2. };
        let c = self.c(color);
        self.eyes(cx, h / 2., 10., 4.5, 4.5, c, beat);
        if let Some(r) = next {
            let remaining = r.remaining(epoch());
            let label = format!(
                "{}  ·  {}",
                if remaining == 0 {
                    self.tr("Acum", "Now").to_owned()
                } else {
                    reminder::countdown(remaining)
                },
                r.text
            );
            self.canvas.text(
                &label,
                R::new(66., 0., w - 80., h),
                12.,
                Face::Text,
                c,
                Align::Left,
            );
        }
    }

    unsafe fn draw_open(&mut self) {
        let text = self.color("text");
        let dim = self.color("dim");
        let accent = self.color("accent");
        let pill = hex(&self.tokens.colors.pill);

        // Header: mini eyes, tabs, clock.
        let eye_color = self.c(hex(&self.tokens.colors.visualizer_bar));
        self.eyes(30., 26., 7., 3.4, 3.4, eye_color, 0.);
        let mut x = 58.;
        let pages = PAGES;
        for p in pages {
            let r = R::new(x, 11., 36., 30.);
            let active = p == self.page;
            let hover = self.mouse.is_some_and(|m| r.contains(m));
            if active {
                self.canvas.rrect(r, 10., self.c(pill));
            } else if hover {
                self.canvas.rrect(r, 10., self.c(fade(pill, 0.7)));
            }
            let glyph = match p {
                Page::Music => I_MUSIC,
                Page::Focus => I_TIMER,
                Page::Water => '\0',
                Page::Reminders => I_BELL,
                Page::Shelf => I_SHELF,
                Page::Notes => I_NOTE,
                Page::Clipboard => I_CLIP,
                Page::Folder => I_FOLDER,
            };
            let col = if active { accent } else { dim };
            if glyph == '\0' {
                self.canvas.drop(r.cx(), r.cy(), 15., self.c(col));
            } else {
                self.canvas.icon(glyph, r, 15., self.c(col));
            }
            // Small counters on the data tabs.
            let count = match p {
                Page::Shelf => self.shelf.len(),
                Page::Clipboard => self.clips.len(),
                _ => 0,
            };
            if count > 0 && !active {
                self.canvas
                    .ellipse(r.x + r.w - 7., r.y + 7., 3., 3., self.c(accent));
            }
            self.hits.push((r, Action::Tab(p)));
            x += 40.;
        }
        let clock = self.clock.clone();
        let date = self.date.clone();
        self.canvas.text(
            &clock,
            R::new(OPEN_W - 120., 10., 104., 20.),
            15.,
            Face::Bold,
            self.c(text),
            Align::Right,
        );
        self.canvas.text(
            &date,
            R::new(OPEN_W - 120., 28., 104., 16.),
            11.,
            Face::Text,
            self.c(dim),
            Align::Right,
        );
        let sep = self.color("sep");
        self.canvas.line(
            (18., HEADER_H + 2.),
            (OPEN_W - 18., HEADER_H + 2.),
            self.c(sep),
            1.,
        );

        let body = R::new(18., HEADER_H + 10., OPEN_W - 36., OPEN_H - HEADER_H - 22.);
        match self.page {
            Page::Music => self.draw_music(body),
            Page::Focus => self.draw_focus(body),
            Page::Water => self.draw_water(body),
            Page::Reminders => self.draw_reminders(body),
            Page::Shelf => self.draw_shelf(body),
            Page::Notes => self.draw_notes(body),
            Page::Clipboard => self.draw_clips(body),
            Page::Folder => self.draw_folder(body),
        }

        // Toast / error pill at the bottom.
        let msg = self
            .error
            .clone()
            .or_else(|| self.toast.as_ref().map(|t| t.0.clone()));
        if let Some(msg) = msg {
            let tw = self.canvas.measure(&msg, 12., Face::Text) + 28.;
            let r = R::new((OPEN_W - tw) / 2., OPEN_H - 34., tw, 24.);
            self.canvas.rrect(r, 12., self.c(0xf0262630));
            let col = if self.error.is_some() {
                0xffff8a80
            } else {
                text
            };
            self.canvas
                .text(&msg, r, 12., Face::Text, self.c(col), Align::Center);
        }
    }

    /// A rounded button with an icon (and optional label). Registers a hit.
    #[allow(clippy::too_many_arguments)]
    unsafe fn button(
        &mut self,
        r: R,
        glyph: char,
        label: &str,
        action: Action,
        filled: Argb,
        fg: Argb,
        round: bool,
    ) {
        let hover = self.mouse.is_some_and(|m| r.contains(m));
        let pressed = hover
            && self
                .press
                .as_ref()
                .is_some_and(|p| p.action.as_ref() == Some(&action));
        let bg = if hover {
            brighten(filled, 0.12)
        } else {
            filled
        };
        let r2 = if pressed { r.inset(1.5) } else { r };
        let radius = if round { r2.h / 2. } else { 12. };
        self.canvas.rrect(r2, radius, self.c(bg));
        if label.is_empty() {
            self.canvas
                .icon(glyph, r2, (r2.h * 0.4).min(20.), self.c(fg));
        } else {
            let lw = self.canvas.measure(label, 13., Face::Text);
            let total = 16. + 8. + lw;
            let x0 = r2.cx() - total / 2.;
            self.canvas
                .icon(glyph, R::new(x0, r2.y, 16., r2.h), 14., self.c(fg));
            self.canvas.text(
                label,
                R::new(x0 + 24., r2.y, lw + 6., r2.h),
                13.,
                Face::Text,
                self.c(fg),
                Align::Left,
            );
        }
        self.hits.push((r, action));
    }

    unsafe fn draw_music(&mut self, b: R) {
        let text = self.color("text");
        let dim = self.color("dim");
        let pill = hex(&self.tokens.colors.pill);
        let accent = self.color("accent");
        self.canvas.text(
            self.tr("Controlează playerul activ", "Control the active player"),
            R::new(b.x, b.y + 4., b.w, 22.),
            13.,
            Face::Text,
            self.c(dim),
            Align::Center,
        );
        let cy = b.y + b.h / 2. + 6.;
        let cx = b.cx();
        self.button(
            R::new(cx - 112., cy - 24., 48., 48.),
            I_PREV,
            "",
            Action::MediaPrev,
            pill,
            text,
            true,
        );
        self.button(
            R::new(cx - 34., cy - 34., 68., 68.),
            I_PLAY,
            "",
            Action::MediaPlay,
            accent,
            0xffffffff,
            true,
        );
        self.button(
            R::new(cx + 64., cy - 24., 48., 48.),
            I_NEXT,
            "",
            Action::MediaNext,
            pill,
            text,
            true,
        );
    }

    unsafe fn draw_focus(&mut self, b: R) {
        let text = self.color("text");
        let dim = self.color("dim");
        let pill = hex(&self.tokens.colors.pill);
        let focus = hex(&self.tokens.colors.focus);
        let snap = self.focus.snapshot();
        let cfg = self.config.pomodoro_config();
        let total = match snap.phase {
            mira_core::model::PomodoroPhase::Work => cfg.work_secs,
            mira_core::model::PomodoroPhase::ShortBreak => cfg.short_break_secs,
            mira_core::model::PomodoroPhase::LongBreak => cfg.long_break_secs,
        }
        .max(1);
        let frac = 1. - snap.remaining_secs as f32 / total as f32;
        let (cx, cy, r) = (b.x + 90., b.cy(), 62.);
        self.canvas
            .arc(cx, cy, r, 0., 360., self.c(fade(focus, 0.18)), 9.);
        self.canvas
            .arc(cx, cy, r, -90., (frac * 360.).max(0.5), self.c(focus), 9.);
        let time = format!(
            "{:02}:{:02}",
            snap.remaining_secs / 60,
            snap.remaining_secs % 60
        );
        self.canvas.text(
            &time,
            R::new(cx - 60., cy - 18., 120., 30.),
            26.,
            Face::Bold,
            self.c(text),
            Align::Center,
        );
        let phase = match snap.phase {
            mira_core::model::PomodoroPhase::Work => self.tr("Concentrare", "Focus"),
            mira_core::model::PomodoroPhase::ShortBreak => self.tr("Pauză scurtă", "Short break"),
            mira_core::model::PomodoroPhase::LongBreak => self.tr("Pauză lungă", "Long break"),
        };
        self.canvas.text(
            phase,
            R::new(cx - 60., cy + 12., 120., 18.),
            12.,
            Face::Text,
            self.c(dim),
            Align::Center,
        );
        let right = b.x + 200.;
        let cycles = if self.ro() {
            format!("Pomodoro · {} cicluri", snap.completed_cycles)
        } else {
            format!("Pomodoro · {} cycles", snap.completed_cycles)
        };
        self.canvas.text(
            &cycles,
            R::new(right, b.y + 18., 380., 24.),
            16.,
            Face::Bold,
            self.c(text),
            Align::Left,
        );
        let (glyph, label) = if snap.running {
            (I_PAUSE, self.tr("Pauză", "Pause"))
        } else {
            (I_PLAY, self.tr("Pornește", "Start"))
        };
        let by = b.y + 76.;
        self.button(
            R::new(right, by, 132., 44.),
            glyph,
            label,
            Action::FocusToggle,
            focus,
            0xffffffff,
            true,
        );
        self.button(
            R::new(right + 142., by, 110., 44.),
            I_SKIP,
            self.tr("Sari", "Skip"),
            Action::FocusSkip,
            pill,
            text,
            true,
        );
        self.button(
            R::new(right + 262., by, 110., 44.),
            I_RESET,
            "Reset",
            Action::FocusReset,
            pill,
            text,
            true,
        );
    }

    unsafe fn draw_water(&mut self, b: R) {
        let text = self.color("text");
        let dim = self.color("dim");
        let pill = hex(&self.tokens.colors.pill);
        let water = hex(&self.tokens.colors.water);
        let water_dim = hex(&self.tokens.colors.water_dim);
        let goal = self.config.water_goal.max(1);
        self.canvas.drop(b.x + 34., b.y + 46., 54., self.c(water));
        let big = format!("{} / {}", self.water, goal);
        self.canvas.text(
            &big,
            R::new(b.x + 76., b.y + 14., 200., 40.),
            30.,
            Face::Bold,
            self.c(text),
            Align::Left,
        );
        let ml = if self.ro() {
            format!("{} ml astăzi", self.water * 250)
        } else {
            format!("{} ml today", self.water * 250)
        };
        self.canvas.text(
            &ml,
            R::new(b.x + 78., b.y + 54., 200., 20.),
            13.,
            Face::Text,
            self.c(dim),
            Align::Left,
        );
        // One drop per glass of the goal.
        for i in 0..goal.min(12) {
            let filled = i < self.water;
            let x = b.x + 12. + i as f32 * 30.;
            self.canvas.drop(
                x + 10.,
                b.y + 118.,
                22.,
                self.c(if filled { water } else { water_dim }),
            );
        }
        let frac = (self.water as f32 / goal as f32).min(1.);
        let bar = R::new(b.x + 12., b.y + 150., 356., 8.);
        self.canvas.rrect(bar, 4., self.c(water_dim));
        if frac > 0. {
            self.canvas
                .rrect(R::new(bar.x, bar.y, bar.w * frac, bar.h), 4., self.c(water));
        }
        let rx = b.x + b.w - 190.;
        self.button(
            R::new(rx, b.y + 40., 80., 80.),
            I_REMOVE,
            "",
            Action::Water(-1),
            pill,
            text,
            true,
        );
        self.button(
            R::new(rx + 96., b.y + 40., 80., 80.),
            I_ADD,
            "",
            Action::Water(1),
            water,
            0xffffffff,
            true,
        );
    }

    unsafe fn draw_reminders(&mut self, b: R) {
        let text = self.color("text");
        let dim = self.color("dim");
        let pill = hex(&self.tokens.colors.pill);
        let accent = self.color("accent");
        let current = self.reminders.get(self.selected_reminder).cloned();
        let card = R::new(b.x, b.y, b.w, 118.);
        self.canvas.rrect(card, 18., self.c(pill));
        match &current {
            Some(r) => {
                let urgency = r.urgency(epoch());
                let color = match urgency {
                    Urgency::Due | Urgency::Urgent => 0xffff7878,
                    Urgency::Soon => 0xffffb65c,
                    _ => accent,
                };
                let head = format!("{} / {}", self.selected_reminder + 1, self.reminders.len());
                self.canvas.text(
                    &head,
                    R::new(card.x + 18., card.y + 10., 80., 18.),
                    11.,
                    Face::Text,
                    self.c(dim),
                    Align::Left,
                );
                self.canvas.text(
                    &r.text,
                    R::new(card.x + 18., card.y + 28., card.w - 36., 26.),
                    17.,
                    Face::Bold,
                    self.c(text),
                    Align::Left,
                );
                let remaining = r.remaining(epoch());
                let count = if remaining == 0 {
                    self.tr("Acum", "Now").to_owned()
                } else {
                    reminder::countdown(remaining)
                };
                self.canvas.text(
                    &count,
                    R::new(card.x + 18., card.y + 56., card.w - 36., 34.),
                    26.,
                    Face::Bold,
                    self.c(color),
                    Align::Left,
                );
                let bar = R::new(card.x + 18., card.y + 98., card.w - 36., 5.);
                self.canvas.rrect(bar, 2.5, self.c(self.color("sep")));
                let f = r.fraction(epoch()) as f32;
                self.canvas
                    .rrect(R::new(bar.x, bar.y, bar.w * f, bar.h), 2.5, self.c(color));
            }
            None => {
                self.canvas.icon(
                    I_BELL,
                    R::new(card.x, card.y + 20., card.w, 40.),
                    28.,
                    self.c(dim),
                );
                self.canvas.text(
                    self.tr(
                        "Niciun reminder. Adaugă ceva de ținut minte.",
                        "No reminders. Add something to remember.",
                    ),
                    R::new(card.x, card.y + 64., card.w, 24.),
                    13.,
                    Face::Text,
                    self.c(dim),
                    Align::Center,
                );
            }
        }
        let by = card.y + card.h + 12.;
        let mut x = b.x;
        self.button(
            R::new(x, by, 44., 40.),
            I_LEFT,
            "",
            Action::RemPrev,
            pill,
            text,
            true,
        );
        x += 52.;
        self.button(
            R::new(x, by, 130., 40.),
            I_ADD,
            self.tr("Nou", "New"),
            Action::RemNew,
            accent,
            0xffffffff,
            true,
        );
        x += 138.;
        if current.is_some() {
            self.button(
                R::new(x, by, 120., 40.),
                I_SNOOZE,
                "+5 min",
                Action::RemSnooze,
                pill,
                text,
                true,
            );
            x += 128.;
            self.button(
                R::new(x, by, 120., 40.),
                I_CHECK,
                self.tr("Gata", "Done"),
                Action::RemDone,
                pill,
                text,
                true,
            );
        }
        self.button(
            R::new(b.x + b.w - 44., by, 44., 40.),
            I_RIGHT,
            "",
            Action::RemNext,
            pill,
            text,
            true,
        );
    }

    unsafe fn icon_for(&mut self, path: &Path, generic: bool) -> *mut GpBitmap {
        let key = (path.to_path_buf(), generic);
        if let Some(b) = self.icons.get(&key) {
            return *b;
        }
        let icon = com::file_icon(path, generic);
        let bmp = if icon.is_null() {
            null_mut()
        } else {
            let b = gfx::bitmap_from_icon(icon);
            DestroyIcon(icon);
            b
        };
        if self.icons.len() > 600 {
            for b in self.icons.values() {
                gfx::free_bitmap(*b);
            }
            self.icons.clear();
        }
        self.icons.insert(key, bmp);
        bmp
    }

    unsafe fn draw_shelf(&mut self, b: R) {
        let text = self.color("text");
        let dim = self.color("dim");
        let accent = self.color("accent");
        let pill = hex(&self.tokens.colors.pill);
        if self.shelf.is_empty() || self.file_drag {
            let zone = b.inset(4.);
            let col = if self.file_drag {
                accent
            } else {
                self.color("sep")
            };
            let wobble = if self.file_drag {
                (self.clock_t * 5.).sin() * 2.
            } else {
                0.
            };
            self.canvas
                .rrect_stroke(zone.inset(-wobble), 20., self.c(col), 2.);
            if self.file_drag {
                self.canvas.rrect(zone, 20., self.c(fade(accent, 0.10)));
            }
            self.canvas.icon(
                I_SHELF,
                R::new(zone.x, zone.y + 34., zone.w, 50.),
                34.,
                self.c(if self.file_drag { accent } else { dim }),
            );
            let msg = if self.file_drag {
                self.tr("Lasă-le aici", "Drop them here")
            } else {
                self.tr(
                    "Trage fișiere aici — le țin ca scurtături",
                    "Drag files here — I keep them as shortcuts",
                )
            };
            self.canvas.text(
                msg,
                R::new(zone.x, zone.y + 92., zone.w, 26.),
                14.,
                Face::Text,
                self.c(text),
                Align::Center,
            );
            if !self.file_drag {
                return;
            }
        }
        let tile_w = 92.;
        let gap = 10.;
        let content = self.shelf.len() as f32 * (tile_w + gap);
        self.scroll_max = (content - b.w).max(0.);
        let scroll = self
            .scroll
            .get(&(Page::Shelf as u8))
            .copied()
            .unwrap_or(0.)
            .min(self.scroll_max);
        self.canvas
            .clip(R::new(b.x - 4., b.y - 6., b.w + 8., b.h + 12.), 0.);
        let n = self.shelf.len();
        for i in 0..n {
            let (path, added) = (self.shelf[i].path.clone(), self.shelf[i].added);
            let x = b.x + i as f32 * (tile_w + gap) - scroll;
            if x > b.x + b.w || x + tile_w < b.x {
                continue;
            }
            // Drop-in bounce, then a gentle float ("buoyancy").
            let age = added.elapsed().as_secs_f32();
            let drop_in = (1. - bounce_in(age / 0.9)) * -50.;
            let bob = (self.clock_t * 2.1 + i as f32 * 0.8).sin() * 2.2;
            let tile = R::new(x, b.y + 18. + drop_in + bob, tile_w, 128.);
            let hover = self.mouse.is_some_and(|m| tile.contains(m));
            let lift = if hover { -5. } else { 0. };
            let tile = R::new(tile.x, tile.y + lift, tile.w, tile.h);
            // Soft shadow, then the card.
            self.canvas.rrect(
                R::new(tile.x + 2., tile.y + 6., tile.w - 4., tile.h),
                18.,
                self.c(0x40000000),
            );
            self.canvas.rrect(
                tile,
                18.,
                self.c(if hover { brighten(pill, 0.08) } else { pill }),
            );
            let icon = self.icon_for(&path, false);
            let missing = !path.exists();
            self.canvas
                .image(icon, R::new(tile.cx() - 24., tile.y + 14., 48., 48.));
            let name = file_name(&path);
            self.canvas.para(
                &name,
                R::new(tile.x + 8., tile.y + 70., tile.w - 16., 36.),
                12.,
                self.c(if missing { dim } else { text }),
            );
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_uppercase())
                .unwrap_or_else(|| {
                    if path.is_dir() {
                        self.tr("FOLDER", "FOLDER").to_string()
                    } else {
                        String::new()
                    }
                });
            self.canvas.text(
                &ext,
                R::new(tile.x, tile.y + 106., tile.w, 16.),
                10.,
                Face::Bold,
                self.c(dim),
                Align::Center,
            );
            self.hits.push((tile, Action::ShelfOpen(i)));
        }
        self.canvas.unclip();
        if !self.file_drag {
            self.canvas.text(
                self.tr(
                    "Click: deschide · Trage: scoate · Click dreapta: elimină",
                    "Click: open · Drag: take out · Right-click: remove",
                ),
                R::new(b.x, b.y + b.h - 14., b.w, 16.),
                11.,
                Face::Text,
                self.c(dim),
                Align::Center,
            );
        }
    }

    unsafe fn draw_notes(&mut self, b: R) {
        let dark = 0xff2b2410;
        let card_w = 168.;
        let gap = 12.;
        let colors = [0xffffe58a, 0xffb8f2c8, 0xffffc7d9, 0xffc9dcff];
        let n = self.notes.len();
        let content = (n + 1) as f32 * (card_w + gap);
        self.scroll_max = (content - b.w).max(0.);
        let scroll = self
            .scroll
            .get(&(Page::Notes as u8))
            .copied()
            .unwrap_or(0.)
            .min(self.scroll_max);
        self.canvas
            .clip(R::new(b.x - 4., b.y - 6., b.w + 8., b.h + 12.), 0.);
        // "New note" tile first.
        let new_tile = R::new(b.x - scroll, b.y + 6., card_w, b.h - 14.);
        let hover = self.mouse.is_some_and(|m| new_tile.contains(m));
        self.canvas.rrect_stroke(
            new_tile,
            16.,
            self.c(if hover {
                self.color("accent")
            } else {
                self.color("sep")
            }),
            2.,
        );
        self.canvas.icon(
            I_ADD,
            R::new(new_tile.x, new_tile.y + 50., new_tile.w, 40.),
            26.,
            self.c(self.color("text")),
        );
        self.canvas.para(
            self.tr(
                "Notiță nouă\nclick, scrie sau Ctrl+V",
                "New note\nclick, type or Ctrl+V",
            ),
            R::new(new_tile.x + 22., new_tile.y + 96., new_tile.w - 30., 50.),
            12.,
            self.c(self.color("dim")),
        );
        self.hits.push((new_tile, Action::NoteNew));
        for i in 0..n {
            let x = b.x + (i + 1) as f32 * (card_w + gap) - scroll;
            if x > b.x + b.w || x + card_w < b.x {
                continue;
            }
            let tilt = ((i as f32 * 1.7).sin()) * 1.5;
            let card = R::new(x, b.y + 6. + tilt, card_w, b.h - 14.);
            let editing = self.edit == Some(Field::Note(i));
            let col = colors[i % colors.len()];
            self.canvas.rrect(
                R::new(card.x + 2., card.y + 5., card.w - 4., card.h),
                14.,
                self.c(0x50000000),
            );
            self.canvas.rrect(card, 14., self.c(col));
            if editing {
                self.canvas
                    .rrect_stroke(card.inset(-2.), 16., self.c(self.color("accent")), 2.5);
            }
            let mut body = self.notes[i].clone();
            if editing && (self.clock_t * 2.).fract() < 0.5 {
                body.push('▏');
            }
            if body.is_empty() {
                body = self.tr("Scrie ceva…", "Type something…").into();
            }
            self.canvas.para(
                &body,
                R::new(card.x + 12., card.y + 12., card.w - 24., card.h - 44.),
                13.,
                self.c(dark),
            );
            self.hits.push((card, Action::NoteEdit(i)));
            let copy = R::new(card.x + card.w - 64., card.y + card.h - 30., 26., 24.);
            let del = R::new(card.x + card.w - 34., card.y + card.h - 30., 26., 24.);
            self.canvas.rrect(copy, 8., self.c(0x22000000));
            self.canvas.rrect(del, 8., self.c(0x22000000));
            self.canvas.icon(I_COPY, copy, 12., self.c(dark));
            self.canvas.icon(I_DELETE, del, 12., self.c(dark));
            self.hits.push((copy, Action::NoteCopy(i)));
            self.hits.push((del, Action::NoteDelete(i)));
        }
        self.canvas.unclip();
    }

    unsafe fn draw_clips(&mut self, b: R) {
        let text = self.color("text");
        let dim = self.color("dim");
        let pill = hex(&self.tokens.colors.pill);
        if self.clips.is_empty() {
            self.canvas
                .icon(I_CLIP, R::new(b.x, b.y + 30., b.w, 44.), 30., self.c(dim));
            self.canvas.text(
                self.tr(
                    "Tot ce copiezi (Ctrl+C) apare aici",
                    "Everything you copy (Ctrl+C) shows up here",
                ),
                R::new(b.x, b.y + 82., b.w, 24.),
                14.,
                Face::Text,
                self.c(text),
                Align::Center,
            );
            return;
        }
        let head = if self.ro() {
            format!("{} copieri · click = copiază din nou", self.clips.len())
        } else {
            format!("{} copies · click = copy again", self.clips.len())
        };
        self.canvas.text(
            &head,
            R::new(b.x + 4., b.y, b.w - 120., 22.),
            12.,
            Face::Text,
            self.c(dim),
            Align::Left,
        );
        self.button(
            R::new(b.x + b.w - 104., b.y - 2., 104., 26.),
            I_DELETE,
            self.tr("Golește", "Clear"),
            Action::ClipClear,
            pill,
            text,
            true,
        );
        let list = R::new(b.x, b.y + 30., b.w, b.h - 30.);
        let row_h = 38.;
        self.scroll_max = (self.clips.len() as f32 * row_h - list.h).max(0.);
        let scroll = self
            .scroll
            .get(&(Page::Clipboard as u8))
            .copied()
            .unwrap_or(0.)
            .min(self.scroll_max);
        self.canvas.clip(list, 0.);
        let now = SystemTime::now();
        for i in 0..self.clips.len() {
            let y = list.y + i as f32 * row_h - scroll;
            if y > list.y + list.h || y + row_h < list.y {
                continue;
            }
            let r = R::new(list.x, y, list.w, row_h - 4.);
            let hover = self
                .mouse
                .is_some_and(|m| r.contains(m) && list.contains(m));
            self.canvas.rrect(
                r,
                10.,
                self.c(if hover { brighten(pill, 0.08) } else { pill }),
            );
            let (glyph, preview) = match &self.clips[i].clip {
                Clip::Text(t) => (I_DOC, t.split_whitespace().collect::<Vec<_>>().join(" ")),
                Clip::Files(f) => (
                    I_FOLDER,
                    f.iter()
                        .map(|p| file_name(p))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            };
            self.canvas.icon(
                glyph,
                R::new(r.x + 6., r.y, 26., r.h),
                14.,
                self.c(self.color("accent")),
            );
            let ago = now
                .duration_since(self.clips[i].at)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let ago = if ago < 60 {
                self.tr("acum", "now").to_string()
            } else if ago < 3600 {
                format!("{} min", ago / 60)
            } else {
                format!("{} h", ago / 3600)
            };
            self.canvas.text(
                &preview,
                R::new(r.x + 36., r.y, r.w - 110., r.h),
                13.,
                Face::Text,
                self.c(text),
                Align::Left,
            );
            self.canvas.text(
                &ago,
                R::new(r.x + r.w - 70., r.y, 58., r.h),
                11.,
                Face::Text,
                self.c(dim),
                Align::Right,
            );
            self.hits.push((r, Action::ClipCopy(i)));
        }
        self.canvas.unclip();
    }

    unsafe fn draw_folder(&mut self, b: R) {
        let text = self.color("text");
        let dim = self.color("dim");
        let accent = self.color("accent");
        let pill = hex(&self.tokens.colors.pill);

        // Top row: folder chip, search field, run button.
        let chip_label = match (&self.context_loading, &self.context) {
            (Some(p), _) => format!("{}…", file_name(p)),
            (None, Some(c)) => file_name(&c.root),
            _ => self.tr("niciun folder", "no folder").to_string(),
        };
        let chip_w = (self.canvas.measure(&chip_label, 12., Face::Bold) + 44.).min(190.);
        let chip = R::new(b.x, b.y, chip_w, 32.);
        self.canvas.rrect(chip, 16., self.c(pill));
        self.canvas.icon(
            I_FOLDER,
            R::new(chip.x + 6., chip.y, 22., 32.),
            13.,
            self.c(accent),
        );
        self.canvas.text(
            &chip_label,
            R::new(chip.x + 30., chip.y, chip.w - 36., 32.),
            12.,
            Face::Bold,
            self.c(text),
            Align::Left,
        );
        if self.context.is_some() {
            let x = R::new(chip.x + chip.w + 4., chip.y + 4., 24., 24.);
            self.canvas.icon(I_CLOSE, x, 10., self.c(dim));
            self.hits.push((x, Action::FolderClear));
        }
        let field = R::new(chip.x + chip.w + 34., b.y, b.w - chip.w - 34. - 92., 32.);
        let editing = self.edit == Some(Field::Search);
        self.canvas.rrect(field, 16., self.c(0xff15151a));
        self.canvas.rrect_stroke(
            field,
            16.,
            self.c(if editing { accent } else { self.color("sep") }),
            1.5,
        );
        self.canvas.icon(
            I_SEARCH,
            R::new(field.x + 6., field.y, 24., 32.),
            12.,
            self.c(dim),
        );
        let mut q = self.query.clone();
        let placeholder = q.is_empty() && !editing;
        if placeholder {
            q = self
                .tr("Caută în fișiere (grep)…", "Search in files (grep)…")
                .into();
        } else if editing && (self.clock_t * 2.).fract() < 0.5 {
            q.push('▏');
        }
        self.canvas.text(
            &q,
            R::new(field.x + 32., field.y, field.w - 40., 32.),
            13.,
            Face::Text,
            self.c(if placeholder { dim } else { text }),
            Align::Left,
        );
        self.hits.push((field, Action::SearchFocus));
        self.button(
            R::new(b.x + b.w - 84., b.y, 84., 32.),
            I_SEARCH,
            self.tr("Caută", "Find"),
            Action::SearchRun,
            accent,
            0xffffffff,
            true,
        );

        let list = R::new(b.x, b.y + 42., b.w, b.h - 42.);
        let row_h = 34.;
        if let Some(search) = &self.search {
            let hits = search.hits.lock().unwrap().clone();
            let done = search.done.load(Ordering::Relaxed);
            let scanned = search.scanned.load(Ordering::Relaxed);
            let total = search.total.load(Ordering::Relaxed);
            let status = if self.ro() {
                format!(
                    "{} rezultate · {}/{} fișiere{}",
                    hits.len(),
                    scanned,
                    total,
                    if done { "" } else { " · caut…" }
                )
            } else {
                format!(
                    "{} hits · {}/{} files{}",
                    hits.len(),
                    scanned,
                    total,
                    if done { "" } else { " · searching…" }
                )
            };
            self.canvas.text(
                &status,
                R::new(list.x + 4., list.y - 2., list.w, 18.),
                11.,
                Face::Text,
                self.c(dim),
                Align::Left,
            );
            let list = R::new(list.x, list.y + 18., list.w, list.h - 18.);
            self.scroll_max = (hits.len() as f32 * row_h - list.h).max(0.);
            let scroll = self
                .scroll
                .get(&(Page::Folder as u8))
                .copied()
                .unwrap_or(0.)
                .min(self.scroll_max);
            let root = self
                .context
                .as_ref()
                .map(|c| c.root.clone())
                .unwrap_or_default();
            self.canvas.clip(list, 0.);
            for (i, h) in hits.iter().enumerate() {
                let y = list.y + i as f32 * row_h - scroll;
                if y > list.y + list.h || y + row_h < list.y {
                    continue;
                }
                let r = R::new(list.x, y, list.w, row_h - 4.);
                let hover = self
                    .mouse
                    .is_some_and(|m| r.contains(m) && list.contains(m));
                self.canvas.rrect(
                    r,
                    10.,
                    self.c(if hover { brighten(pill, 0.08) } else { pill }),
                );
                let icon = self.icon_for(&h.path, true);
                self.canvas
                    .image(icon, R::new(r.x + 8., r.y + 5., 20., 20.));
                let loc = if h.line > 0 {
                    format!("{}:{}", files::relative(&root, &h.path), h.line)
                } else {
                    files::relative(&root, &h.path)
                };
                let lw = self.canvas.measure(&loc, 12., Face::Bold).min(r.w * 0.45);
                self.canvas.text(
                    &loc,
                    R::new(r.x + 34., r.y, lw + 4., r.h),
                    12.,
                    Face::Bold,
                    self.c(text),
                    Align::Left,
                );
                self.canvas.text(
                    &h.text,
                    R::new(r.x + 46. + lw, r.y, r.w - lw - 56., r.h),
                    12.,
                    Face::Text,
                    self.c(dim),
                    Align::Left,
                );
                self.hits
                    .push((r, Action::FileOpen(h.path.clone(), h.line)));
            }
            self.canvas.unclip();
            return;
        }
        let Some(ctx) = &self.context else {
            self.canvas.icon(
                I_FOLDER,
                R::new(list.x, list.y + 14., list.w, 40.),
                28.,
                self.c(dim),
            );
            self.canvas.text(
                self.tr(
                    "Trage robotelul peste un folder sau lasă un folder aici",
                    "Drag the robot onto a folder, or drop a folder here",
                ),
                R::new(list.x, list.y + 60., list.w, 22.),
                13.,
                Face::Text,
                self.c(text),
                Align::Center,
            );
            return;
        };
        let root = ctx.root.clone();
        let files: Vec<PathBuf> = ctx.files.clone();
        self.scroll_max = (files.len() as f32 * row_h - list.h).max(0.);
        let scroll = self
            .scroll
            .get(&(Page::Folder as u8))
            .copied()
            .unwrap_or(0.)
            .min(self.scroll_max);
        self.canvas.clip(list, 0.);
        let first = (scroll / row_h) as usize;
        let visible = (list.h / row_h) as usize + 2;
        for (i, p) in files.iter().enumerate().skip(first).take(visible) {
            let y = list.y + i as f32 * row_h - scroll;
            let r = R::new(list.x, y, list.w, row_h - 4.);
            let hover = self
                .mouse
                .is_some_and(|m| r.contains(m) && list.contains(m));
            self.canvas.rrect(
                r,
                10.,
                self.c(if hover { brighten(pill, 0.08) } else { pill }),
            );
            let icon = self.icon_for(p, true);
            self.canvas
                .image(icon, R::new(r.x + 8., r.y + 5., 20., 20.));
            let rel = files::relative(&root, p);
            self.canvas.text(
                &rel,
                R::new(r.x + 34., r.y, r.w - 44., r.h),
                12.,
                Face::Text,
                self.c(text),
                Align::Left,
            );
            self.hits.push((r, Action::FileOpen(p.clone(), 0)));
        }
        self.canvas.unclip();
    }

    unsafe fn draw_pet(&mut self) {
        self.content_dx = 0.;
        let (sx, sy) = self.pet.scale();
        let lift = self.pet.lift();
        let (cx, cy) = (PET_WIN_W / 2., PET_WIN_H / 2. - lift);
        // Shadow on the "ground" stays put while it hops.
        let shadow_w = BOX_W * 0.7 * (1. - lift / 80.);
        self.canvas.ellipse(
            PET_WIN_W / 2.,
            PET_WIN_H / 2. + BOX_H / 2. + 12.,
            shadow_w / 2.,
            5.,
            0x38000000,
        );
        self.canvas.push(cx, cy + BOX_H / 2., sx, sy);
        let body = R::new(-BOX_W / 2., -BOX_H, BOX_W, BOX_H);
        let bg = self.color("bg");
        self.canvas.rrect(body, 24., bg);
        self.canvas.rrect_stroke(body, 24., 0x30ffffff, 1.);
        self.canvas.reset();
        let eye = hex(&self.tokens.colors.visualizer_bar);
        self.alpha = 1.;
        // Eyes sit at the centre of the (squashed) body, which is anchored at
        // its bottom edge.
        let eye_y = cy + BOX_H / 2. * (1. - sy) - 2.;
        self.eyes(cx, eye_y, 15. * sx, 7. * sx, 9. * sy, eye, 0.);
        if self.mode == Mode::Dragging {
            let hint = self.tr("lasă-mă pe un folder", "drop me on a folder");
            self.canvas.text(
                hint,
                R::new(0., cy + BOX_H / 2. + 18., PET_WIN_W, 18.),
                11.,
                Face::Text,
                0xccf2f2f3,
                Align::Center,
            );
        }
    }
}

fn brighten(c: Argb, k: f32) -> Argb {
    let ch = |shift: u32| {
        let v = ((c >> shift) & 0xff) as f32;
        ((v + (255. - v) * k) as u32).min(255) << shift
    };
    (c & 0xff00_0000) | ch(16) | ch(8) | ch(0)
}

unsafe fn open_path(p: &Path) {
    ShellExecuteW(
        null_mut(),
        wide("open").as_ptr(),
        wide(&p.to_string_lossy()).as_ptr(),
        null(),
        null(),
        SW_SHOWNORMAL,
    );
}

/// Open a search hit in VS Code at its line when installed, else with the
/// file's default app.
unsafe fn open_at_line(p: &Path, line: usize) {
    if line > 0 {
        let code = std::env::var_os("LOCALAPPDATA")
            .map(|d| {
                PathBuf::from(d)
                    .join("Programs")
                    .join("Microsoft VS Code")
                    .join("Code.exe")
            })
            .filter(|c| c.exists());
        if let Some(code) = code {
            let args = format!("-g \"{}:{}\"", p.to_string_lossy(), line);
            ShellExecuteW(
                null_mut(),
                wide("open").as_ptr(),
                wide(&code.to_string_lossy()).as_ptr(),
                wide(&args).as_ptr(),
                null(),
                SW_SHOWNORMAL,
            );
            return;
        }
    }
    open_path(p);
}

fn lparam_point(lp: LPARAM) -> (i32, i32) {
    (
        (lp & 0xffff) as i16 as i32,
        ((lp >> 16) & 0xffff) as i16 as i32,
    )
}

unsafe fn local_point(hwnd: HWND, lp: LPARAM) -> (POINT, (f32, f32)) {
    let (x, y) = lparam_point(lp);
    let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.;
    let mut screen = POINT { x, y };
    ClientToScreen(hwnd, &mut screen);
    (screen, (x as f32 / scale, y as f32 / scale))
}

unsafe fn after(hwnd: HWND, a: After) {
    match a {
        After::Nothing => {}
        After::DragOut(path) => {
            ReleaseCapture();
            com::drag_out(&path);
        }
        After::Menu => show_menu(hwnd),
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let docked = with_host(|h| h.mode == Mode::Docked).unwrap_or(true);
    let menu = CreatePopupMenu();
    if !docked {
        AppendMenuW(menu, MF_STRING, 2, wide("Acasă / Home").as_ptr());
    }
    AppendMenuW(menu, MF_STRING, 1, wide("Mira — Ieșire / Exit").as_ptr());
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    let choice = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY,
        pt.x,
        pt.y,
        0,
        hwnd,
        null(),
    );
    DestroyMenu(menu);
    match choice {
        1 => {
            DestroyWindow(hwnd);
        }
        2 => {
            with_host(|h| {
                h.mode = Mode::Returning;
                h.dirty = true;
            });
        }
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_OPEN_EDITOR => {
            open_reminder_editor(hwnd);
            0
        }
        WM_TIMER => {
            with_host(|h| h.tick());
            0
        }
        WM_CONTEXT_READY | WM_SEARCH_PROGRESS => {
            with_host(|h| h.dirty = true);
            0
        }
        com::WM_DRAG_ENTER => {
            with_host(|h| {
                h.file_drag = true;
                if h.mode == Mode::Docked {
                    if h.page != Page::Folder {
                        h.page = Page::Shelf;
                    }
                    h.set_open(true);
                }
                h.dirty = true;
            });
            0
        }
        com::WM_DRAG_LEAVE => {
            with_host(|h| {
                h.file_drag = false;
                h.dirty = true;
            });
            0
        }
        com::WM_DROP_FILES => {
            let files = com::PENDING_DROP.with(|p| std::mem::take(&mut *p.borrow_mut()));
            with_host(|h| h.files_dropped(files));
            0
        }
        WM_CLIPBOARDUPDATE => {
            with_host(|h| {
                h.clip_retry = 10;
                h.capture_clipboard();
            });
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
        WM_ACTIVATE => {
            if (wp & 0xffff) as u32 == WA_INACTIVE {
                with_host(|h| h.end_edit());
            }
            0
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            let (screen, local) = local_point(hwnd, lp);
            let home =
                msg == WM_LBUTTONDBLCLK && with_host(|h| h.mode == Mode::Pet).unwrap_or(false);
            if home {
                with_host(|h| {
                    h.mode = Mode::Returning;
                    h.press = None;
                    h.dirty = true;
                });
            } else {
                SetCapture(hwnd);
                with_host(|h| h.mouse_down(screen, local));
            }
            0
        }
        WM_MOUSEMOVE => {
            let (screen, _) = local_point(hwnd, lp);
            if let Some(a) = with_host(|h| h.mouse_move(screen)) {
                after(hwnd, a);
            }
            0
        }
        WM_LBUTTONUP => {
            let (screen, _) = local_point(hwnd, lp);
            ReleaseCapture();
            with_host(|h| h.mouse_up(screen));
            0
        }
        WM_RBUTTONUP => {
            let (_, local) = local_point(hwnd, lp);
            if let Some(a) = with_host(|h| h.right_click(local)) {
                after(hwnd, a);
            }
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp >> 16) & 0xffff) as i16;
            with_host(|h| h.wheel(delta));
            0
        }
        WM_CHAR => {
            with_host(|h| h.key_char(wp as u16));
            0
        }
        WM_KEYDOWN => {
            let handled = with_host(|h| h.key_down(wp as u16)).unwrap_or(false);
            if handled {
                0
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            with_host(|h| h.dirty = true);
            0
        }
        WM_DESTROY => {
            KillTimer(hwnd, 1);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

pub unsafe fn run() {
    SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    // A named window prevents two copies (including installer auto-launch).
    let class = wide("MiraNotchWindow");
    if !FindWindowW(class.as_ptr(), null()).is_null() {
        return;
    }
    OleInitialize(null());
    let instance = GetModuleHandleW(null());
    let wc = WNDCLASSW {
        style: CS_DBLCLKS,
        lpfnWndProc: Some(wndproc),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        hCursor: LoadCursorW(null_mut(), IDC_HAND),
        ..std::mem::zeroed()
    };
    let editor_class = wide("MiraReminderEditor");
    let editor_wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(editor_proc),
        lpszClassName: editor_class.as_ptr(),
        hbrBackground: (COLOR_WINDOW + 1) as HBRUSH,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        ..wc
    };
    RegisterClassW(&editor_wc);
    if RegisterClassW(&wc) == 0 {
        return;
    }
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class.as_ptr(),
        wide("Mira").as_ptr(),
        WS_POPUP,
        0,
        0,
        180,
        32,
        null_mut(),
        null_mut(),
        instance,
        null(),
    );
    if hwnd.is_null() {
        return;
    }
    let state = match Host::new(hwnd) {
        Ok(state) => state,
        Err(e) => {
            MessageBoxW(
                null_mut(),
                wide(&e.to_string()).as_ptr(),
                wide("Mira startup failed").as_ptr(),
                MB_ICONERROR,
            );
            DestroyWindow(hwnd);
            return;
        }
    };
    HOST.with(|s| *s.borrow_mut() = Some(state));
    com::register_drop_target(hwnd);
    AddClipboardFormatListener(hwnd);
    with_host(|h| h.tick());
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    SetTimer(hwnd, 1, 16, None);
    let mut message: MSG = std::mem::zeroed();
    while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
        let editor = FindWindowW(editor_class.as_ptr(), null());
        if !editor.is_null() && IsDialogMessageW(editor, &message) != 0 {
            continue;
        }
        TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    HOST.with(|s| *s.borrow_mut() = None);
}

// --- reminder editor (a plain native dialog) ------------------------------------------

unsafe fn open_reminder_editor(owner: HWND) {
    let class = wide("MiraReminderEditor");
    let existing = FindWindowW(class.as_ptr(), null());
    if !existing.is_null() {
        SetForegroundWindow(existing);
        return;
    }
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        class.as_ptr(),
        wide("Mira · Reminder").as_ptr(),
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        440,
        230,
        owner,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
    SetFocus(GetDlgItem(hwnd, 101));
}

unsafe fn control(
    hwnd: HWND,
    kind: &str,
    text: &str,
    id: usize,
    rect: (i32, i32, i32, i32),
    style: u32,
) {
    let (x, y, w, h) = rect;
    let child = CreateWindowExW(
        0,
        wide(kind).as_ptr(),
        wide(text).as_ptr(),
        WS_CHILD | WS_VISIBLE | style,
        x,
        y,
        w,
        h,
        hwnd,
        id as HMENU,
        GetModuleHandleW(null()),
        null(),
    );
    SendMessageW(
        child,
        WM_SETFONT,
        GetStockObject(DEFAULT_GUI_FONT) as usize,
        1,
    );
}

unsafe fn edit_text(hwnd: HWND, id: i32) -> String {
    let child = GetDlgItem(hwnd, id);
    let mut value = vec![0u16; GetWindowTextLengthW(child) as usize + 1];
    let len = GetWindowTextW(child, value.as_mut_ptr(), value.len() as i32);
    String::from_utf16_lossy(&value[..len as usize])
}

unsafe extern "system" fn editor_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            control(
                hwnd,
                "STATIC",
                "Ce vrei să-ți amintești? / Reminder title",
                100,
                (16, 14, 395, 20),
                0,
            );
            control(
                hwnd,
                "EDIT",
                "",
                101,
                (16, 38, 395, 26),
                WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            );
            SendMessageW(GetDlgItem(hwnd, 101), EM_SETLIMITTEXT, 200, 0);
            control(
                hwnd,
                "STATIC",
                "În câte minute? / Minutes from now",
                102,
                (16, 76, 300, 20),
                0,
            );
            control(
                hwnd,
                "EDIT",
                "15",
                103,
                (16, 100, 100, 26),
                WS_BORDER | WS_TABSTOP | ES_NUMBER as u32,
            );
            control(
                hwnd,
                "BUTTON",
                "Salvează / Save",
                1,
                (145, 140, 140, 28),
                WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            );
            control(
                hwnd,
                "BUTTON",
                "Anulează / Cancel",
                2,
                (295, 140, 115, 28),
                WS_TABSTOP,
            );
            0
        }
        WM_COMMAND => {
            match wp & 0xffff {
                1 => {
                    let title = edit_text(hwnd, 101);
                    let minutes = edit_text(hwnd, 103)
                        .parse::<i64>()
                        .ok()
                        .filter(|n| (1..=527040).contains(n));
                    let result = if let Some(minutes) = minutes {
                        with_host(|s| {
                            let now = epoch();
                            let result = s.store.add_reminder(&title, now + minutes * 60, now);
                            if result.is_ok() {
                                s.reload_reminders();
                            }
                            result.map(|_| ())
                        })
                        .unwrap_or_else(|| Err(mira_core::Error::other("Mira is busy")))
                    } else {
                        Err(mira_core::Error::other(
                            "Alege 1–527040 minute / Choose 1–527040 minutes",
                        ))
                    };
                    match result {
                        Ok(()) => {
                            DestroyWindow(hwnd);
                        }
                        Err(e) => {
                            MessageBoxW(
                                hwnd,
                                wide(&e.to_string()).as_ptr(),
                                wide("Mira").as_ptr(),
                                MB_ICONWARNING,
                            );
                        }
                    }
                }
                2 => {
                    DestroyWindow(hwnd);
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
