use std::cell::RefCell;
use std::fmt;
use std::io::{self, Write};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::Thread;
use std::time::{Duration, Instant};

use crate::clock::Clock;
use crate::id::ElementId;
use crate::ring::{Consumer, Producer, spsc};
use crate::time::Timestamp;

/// Log severity, most severe first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Level {
    pub const fn rank(self) -> u8 {
        match self {
            Level::Error => 1,
            Level::Warn => 2,
            Level::Info => 3,
            Level::Debug => 4,
            Level::Trace => 5,
        }
    }

    pub const fn from_rank(r: u8) -> Option<Level> {
        Some(match r {
            1 => Level::Error,
            2 => Level::Warn,
            3 => Level::Info,
            4 => Level::Debug,
            5 => Level::Trace,
            _ => return None,
        })
    }

    /// Fixed-width uppercase tag for drain output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }

    /// The canonical uppercase display name.
    pub fn name(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }

    pub fn from_name(s: &str) -> Option<Level> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "error" | "err" => Level::Error,
            "warn" | "warning" => Level::Warn,
            "info" => Level::Info,
            "debug" => Level::Debug,
            "trace" => Level::Trace,
            _ => return None,
        })
    }
}

/// A runtime-adjustable global verbosity gate, one relaxed atomic where `0` is off.
pub struct LevelFilter {
    threshold: AtomicU8,
}

impl LevelFilter {
    pub const fn new() -> Self {
        Self { threshold: AtomicU8::new(0) }
    }

    pub fn with_level(level: Level) -> Self {
        let f = Self::new();
        f.set(Some(level));
        f
    }

    pub fn set(&self, level: Option<Level>) {
        self.threshold.store(level.map_or(0, Level::rank), Relaxed);
    }

    pub fn level(&self) -> Option<Level> {
        Level::from_rank(self.threshold.load(Relaxed))
    }

    #[inline]
    pub fn enabled(&self, level: Level) -> bool {
        level.rank() <= self.threshold.load(Relaxed)
    }
}

impl Default for LevelFilter {
    fn default() -> Self {
        Self::new()
    }
}

/// A parsed `PROFLUENS_DEBUG` value with a global rank and per-target ranks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DebugSpec {
    pub global: Option<u8>,
    pub targets: Vec<(String, u8)>,
}

impl DebugSpec {
    /// Parse comma-separated tokens, each a bare level or `name:level`, skipping bad ones.
    // Parses the env var once at startup.
    #[allow(clippy::disallowed_methods)]
    pub fn parse(s: &str) -> DebugSpec {
        let mut spec = DebugSpec::default();
        for tok in s.split(',') {
            let tok = tok.trim();
            if tok.is_empty() {
                continue;
            }
            match tok.split_once(':') {
                Some((name, lvl)) => {
                    if let Some(rank) = parse_level_token(lvl) {
                        spec.targets.push((name.trim().to_string(), rank));
                    }
                }
                None => {
                    if let Some(rank) = parse_level_token(tok) {
                        spec.global = Some(rank);
                    }
                }
            }
        }
        spec
    }
}

pub fn parse_level_token(s: &str) -> Option<u8> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u8>() {
        return (n <= 5).then_some(n);
    }
    if s.eq_ignore_ascii_case("off") || s.eq_ignore_ascii_case("none") {
        return Some(0);
    }
    Level::from_name(s).map(Level::rank)
}

#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub key: &'static str,
    pub val: FieldValue,
}

impl Field {
    pub const fn new(key: &'static str, val: FieldValue) -> Self {
        Self { key, val }
    }
}

const EMPTY_FIELD: Field = Field { key: "", val: FieldValue::Uint(0) };

#[derive(Clone, Copy, Debug)]
pub enum FieldValue {
    Int(i64),
    Uint(u64),
    Float(f64),
    Bool(bool),
    Str(&'static str),
    Fourcc([u8; 4]),
    Time(Timestamp),
}

impl fmt::Display for FieldValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FieldValue::Int(v) => write!(f, "{v}"),
            FieldValue::Uint(v) => write!(f, "{v}"),
            FieldValue::Float(v) => write!(f, "{v}"),
            FieldValue::Bool(v) => write!(f, "{v}"),
            FieldValue::Str(s) => write!(f, "{s}"),
            FieldValue::Time(t) => match t.nanos() {
                Some(n) => write!(f, "{n}ns"),
                None => write!(f, "none"),
            },
            FieldValue::Fourcc(b) => {
                for &c in b {
                    write!(f, "{}", c as char)?;
                }
                Ok(())
            }
        }
    }
}

macro_rules! from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for FieldValue {
            fn from(v: $t) -> Self { FieldValue::Int(v as i64) }
        }
    )*};
}
macro_rules! from_uint {
    ($($t:ty),*) => {$(
        impl From<$t> for FieldValue {
            fn from(v: $t) -> Self { FieldValue::Uint(v as u64) }
        }
    )*};
}
from_int!(i8, i16, i32, i64, isize);
from_uint!(u8, u16, u32, u64, usize);

impl From<f64> for FieldValue {
    fn from(v: f64) -> Self {
        FieldValue::Float(v)
    }
}

impl From<f32> for FieldValue {
    fn from(v: f32) -> Self {
        FieldValue::Float(f64::from(v))
    }
}

impl From<bool> for FieldValue {
    fn from(v: bool) -> Self {
        FieldValue::Bool(v)
    }
}
impl From<&'static str> for FieldValue {
    fn from(v: &'static str) -> Self {
        FieldValue::Str(v)
    }
}
impl From<[u8; 4]> for FieldValue {
    fn from(v: [u8; 4]) -> Self {
        FieldValue::Fourcc(v)
    }
}
impl From<Timestamp> for FieldValue {
    fn from(v: Timestamp) -> Self {
        FieldValue::Time(v)
    }
}
impl From<ElementId> for FieldValue {
    fn from(v: ElementId) -> Self {
        FieldValue::Uint(v.0 as u64)
    }
}

pub const MAX_FIELDS: usize = 4;

/// Bytes of free text a record can carry.
pub const TEXT_CAP: usize = 256;

/// Bounded inline text for what fields cannot say, a call dump or a ladder at trace level.
/// Written at the emit site, cut at [`TEXT_CAP`] on a character boundary.
#[derive(Clone, Copy)]
pub struct Text {
    len: u16,
    bytes: [u8; TEXT_CAP],
}

impl Text {
    pub const EMPTY: Text = Text { len: 0, bytes: [0; TEXT_CAP] };

    /// The formatted arguments, cut to fit.
    pub fn of(args: fmt::Arguments<'_>) -> Text {
        let mut t = Text::EMPTY;
        // A full buffer is the one error, the text is kept as far as it fits.
        let _ = fmt::Write::write_fmt(&mut t, args);
        t
    }

    pub fn as_str(&self) -> &str {
        // Only whole characters land in the buffer, see `write_str`.
        std::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Write for Text {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = TEXT_CAP - self.len as usize;
        let mut take = s.len().min(room);
        while !s.is_char_boundary(take) {
            take -= 1;
        }
        self.bytes[self.len as usize..self.len as usize + take]
            .copy_from_slice(&s.as_bytes()[..take]);
        self.len += take as u16;
        if take < s.len() { Err(fmt::Error) } else { Ok(()) }
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl PartialEq for Text {
    fn eq(&self, other: &Text) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Text {}

/// One log line as a fixed-size POD record, formatted only at the drain.
#[derive(Clone, Copy, Debug)]
pub struct LogRecord {
    pub ts: Timestamp,
    /// The application's id for the pipeline, zero when none was set.
    pub session: u64,
    pub element: ElementId,
    pub name: &'static str,
    pub level: Level,
    pub event: &'static str,
    pub nfields: u8,
    pub fields: [Field; MAX_FIELDS],
    pub text: Text,
}

impl LogRecord {
    pub fn fields(&self) -> &[Field] {
        &self.fields[..self.nfields as usize]
    }
}

pub struct LogSink {
    tx: Producer<LogRecord>,
    dropped: Arc<AtomicU64>,
}

impl LogSink {
    /// Enqueue a record, or drop and count it when the ring is full.
    pub fn push(&self, rec: LogRecord) {
        if self.tx.try_push(rec).is_err() {
            self.dropped.fetch_add(1, Relaxed);
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Relaxed)
    }
}

pub struct LogDrain {
    rx: Consumer<LogRecord>,
    dropped: Arc<AtomicU64>,
    /// Drops already said by a notice.
    reported: u64,
    /// The last record taken, the origin a drop notice is stamped with.
    last: Option<LogRecord>,
}

impl LogDrain {
    /// Hand every queued record to `emit`, then a Warn `log.dropped` record with `records=N`
    /// when the ring dropped N since the last notice. True when anything was emitted.
    pub fn sweep(&mut self, emit: impl FnMut(&LogRecord)) -> bool {
        self.sweep_up_to(usize::MAX, emit)
    }

    /// `sweep`, taking at most `most` records.
    pub(crate) fn sweep_up_to(&mut self, most: usize, mut emit: impl FnMut(&LogRecord)) -> bool {
        // Read before taking, a drop counted here was of a full ring, so a record is taken.
        let dropped = self.dropped.load(Relaxed);
        let mut progressed = false;
        for _ in 0..most {
            let Some(rec) = self.rx.try_pop() else { break };
            emit(&rec);
            self.last = Some(rec);
            progressed = true;
        }
        if let Some(last) = self.last.filter(|_| dropped > self.reported) {
            let mut fields = [EMPTY_FIELD; MAX_FIELDS];
            fields[0] = Field::new("records", FieldValue::Uint(dropped - self.reported));
            emit(&LogRecord {
                level: Level::Warn,
                event: "log.dropped",
                nfields: 1,
                fields,
                text: Text::EMPTY,
                ..last
            });
            self.reported = dropped;
            progressed = true;
        }
        progressed
    }

    pub fn try_next(&self) -> Option<LogRecord> {
        self.rx.try_pop()
    }

    /// Block until the next record, or `None` once the sink is dropped and drained.
    pub fn next_blocking(&self) -> Option<LogRecord> {
        self.rx.pop()
    }

    /// Whether the sink was dropped. Records may remain until `try_next` returns `None`.
    pub fn is_closed(&self) -> bool {
        self.rx.is_closed()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Relaxed)
    }
}

pub fn log_channel(capacity: usize) -> (LogSink, LogDrain) {
    let (tx, rx) = spsc::<LogRecord>(capacity);
    let dropped = Arc::new(AtomicU64::new(0));
    (LogSink { tx, dropped: dropped.clone() }, LogDrain { rx, dropped, reported: 0, last: None })
}

/// A log handle that stamps records with the current element.
pub struct Log {
    sink: LogSink,
    filter: Arc<LevelFilter>,
    session: u64,
    element: ElementId,
    name: &'static str,
    start: Instant,
    clock: Option<Arc<dyn Clock>>,
}

impl Log {
    // Wall stamps for a threaded run, a stepped run sets a clock.
    #[allow(clippy::disallowed_methods)]
    pub fn new(sink: LogSink, filter: Arc<LevelFilter>) -> Self {
        Self {
            sink,
            filter,
            session: 0,
            element: ElementId(0),
            name: "",
            start: Instant::now(),
            clock: None,
        }
    }

    pub fn set_element(&mut self, element: ElementId, name: &'static str) {
        self.element = element;
        self.name = name;
    }

    /// Stamps every record from here on with the application's id for the pipeline.
    pub fn set_session(&mut self, session: u64) {
        self.session = session;
    }

    pub fn element(&self) -> ElementId {
        self.element
    }

    pub fn filter(&self) -> &LevelFilter {
        &self.filter
    }

    pub fn dropped(&self) -> u64 {
        self.sink.dropped()
    }

    pub(crate) fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = Some(clock);
    }

    fn now(&self) -> Timestamp {
        match &self.clock {
            Some(clock) => clock.now(),
            None => Timestamp::from_nanos(self.start.elapsed().as_nanos() as u64),
        }
    }
}

/// The emit surface the `log!` macro targets.
pub trait Loggable {
    fn enabled(&self, level: Level) -> bool;
    fn emit(&self, level: Level, event: &'static str, fields: &[Field], text: Text);
}

impl Loggable for Log {
    #[inline]
    fn enabled(&self, level: Level) -> bool {
        self.filter.enabled(level)
    }

    fn emit(&self, level: Level, event: &'static str, fields: &[Field], text: Text) {
        let n = fields.len().min(MAX_FIELDS);
        let mut buf = [EMPTY_FIELD; MAX_FIELDS];
        buf[..n].copy_from_slice(&fields[..n]);
        self.sink.push(LogRecord {
            ts: self.now(),
            session: self.session,
            element: self.element,
            name: self.name,
            level,
            event,
            nfields: n as u8,
            fields: buf,
            text,
        });
    }
}

/// Emit a structured record if the level gate is open, evaluating no arguments otherwise. After
/// the fields, `; "fmt", args` adds bounded free text ([`Text`]) for what fields cannot say.
#[macro_export]
macro_rules! log {
    ($target:expr, $level:expr, $event:expr $(, $key:ident = $val:expr)* ; $($text:tt)+) => {{
        const _: () = assert!(
            <[&str]>::len(&[$(::core::stringify!($key)),*]) <= $crate::log::MAX_FIELDS,
            "a log record holds at most MAX_FIELDS fields"
        );
        let __log = $target;
        let __lvl = $level;
        if $crate::log::Loggable::enabled(__log, __lvl) {
            $crate::log::Loggable::emit(
                __log,
                __lvl,
                $event,
                &[$($crate::log::Field::new(
                    ::core::stringify!($key),
                    $crate::log::FieldValue::from($val),
                )),*],
                $crate::log::Text::of(::core::format_args!($($text)+)),
            );
        }
    }};
    ($target:expr, $level:expr, $event:expr $(, $key:ident = $val:expr)* $(,)?) => {{
        const _: () = assert!(
            <[&str]>::len(&[$(::core::stringify!($key)),*]) <= $crate::log::MAX_FIELDS,
            "a log record holds at most MAX_FIELDS fields"
        );
        let __log = $target;
        let __lvl = $level;
        if $crate::log::Loggable::enabled(__log, __lvl) {
            $crate::log::Loggable::emit(
                __log,
                __lvl,
                $event,
                &[$($crate::log::Field::new(
                    ::core::stringify!($key),
                    $crate::log::FieldValue::from($val),
                )),*],
                $crate::log::Text::EMPTY,
            );
        }
    }};
}

/// Takes a pipeline's drained records and build lines in place of stderr, for an embedding
/// application.
pub trait LogWriter: Send + Sync {
    fn record(&self, rec: &LogRecord);
    fn line(&self, level: Level, msg: fmt::Arguments<'_>);
}

/// An application's log setup, which `Pipeline::new` takes from the calling thread's [`with_logs`].
#[derive(Clone, Default)]
pub struct Logs {
    pub writer: Option<Arc<dyn LogWriter>>,
    pub debug: Option<DebugSpec>,
}

thread_local! {
    static SCOPED: RefCell<Option<Logs>> = const { RefCell::new(None) };
}

/// Run `f` with `logs` as the setup of every `Pipeline::new` on this thread.
pub fn with_logs<R>(logs: &Logs, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Logs>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0.take();
            SCOPED.with(|s| *s.borrow_mut() = prev);
        }
    }
    let _restore = Restore(SCOPED.with(|s| s.borrow_mut().replace(logs.clone())));
    f()
}

pub fn current_logs() -> Logs {
    SCOPED.with(|s| s.borrow().clone().unwrap_or_default())
}

pub fn format_record(w: &mut impl Write, rec: &LogRecord) -> io::Result<()> {
    format_record_styled(w, rec, false)
}

const SGR_RESET: &str = "\x1b[0m";
const SGR_DIM: &str = "\x1b[2m";

pub(crate) fn level_sgr(level: Level) -> &'static str {
    match level {
        Level::Error => "\x1b[1;31m",
        Level::Warn => "\x1b[33m",
        Level::Info => "\x1b[32m",
        Level::Debug => "\x1b[36m",
        Level::Trace => "\x1b[2m",
    }
}

fn element_sgr(id: ElementId) -> &'static str {
    const PALETTE: [&str; 6] =
        ["\x1b[94m", "\x1b[95m", "\x1b[96m", "\x1b[34m", "\x1b[35m", "\x1b[36m"];
    PALETTE[id.0 as usize % PALETTE.len()]
}

/// Whether stderr drains emit ANSI, off under `NO_COLOR`, else when stderr is a terminal.
pub fn stderr_colors_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    use std::io::IsTerminal;
    io::stderr().is_terminal()
}

/// `@session` after the element, nothing for zero.
struct Session(u64);

impl fmt::Display for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 { Ok(()) } else { write!(f, "@{}", self.0) }
    }
}

/// [`format_record`] with optional ANSI styling.
pub fn format_record_styled(w: &mut impl Write, rec: &LogRecord, styled: bool) -> io::Result<()> {
    let ms = rec.ts.nanos().map(|n| n as f64 / 1e6).unwrap_or(f64::NAN);
    let name = if rec.name.is_empty() { "elem" } else { rec.name };
    if styled {
        write!(
            w,
            "{SGR_DIM}[{ms:>10.3}ms]{SGR_RESET} {}{:<5}{SGR_RESET} {}{name}#{}{}{SGR_RESET} {}",
            level_sgr(rec.level),
            rec.level.as_str(),
            element_sgr(rec.element),
            rec.element.0,
            Session(rec.session),
            rec.event
        )?;
        for f in rec.fields() {
            write!(w, " {SGR_DIM}{}={SGR_RESET}{}", f.key, f.val)?;
        }
        if !rec.text.is_empty() {
            write!(w, " {}", Escaped(rec.text.as_str()))?;
        }
    } else {
        write!(
            w,
            "[{ms:>10.3}ms] {:<5} {name}#{}{} {}",
            rec.level.as_str(),
            rec.element.0,
            Session(rec.session),
            rec.event
        )?;
        for f in rec.fields() {
            write!(w, " {}={}", f.key, f.val)?;
        }
        if !rec.text.is_empty() {
            write!(w, " {}", Escaped(rec.text.as_str()))?;
        }
    }
    writeln!(w)
}

/// Log text with its control characters written as `\x..`, so a string lifted from a media
/// file cannot drive the terminal or forge a line.
struct Escaped<'a>(&'a str);

impl fmt::Display for Escaped<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.0.chars() {
            match c {
                '\t' => f.write_str("\\t")?,
                c if c.is_control() => write!(f, "\\x{:02x}", c as u32)?,
                c => fmt::Write::write_char(f, c)?,
            }
        }
        Ok(())
    }
}

/// One run's log drains and where their records go.
pub(crate) struct Drains {
    pub(crate) held: Vec<LogDrain>,
    /// Drains of the elements a change adds while the run goes on.
    pub(crate) joining: Consumer<LogDrain>,
    pub(crate) writer: Option<Arc<dyn LogWriter>>,
    pub(crate) styled: bool,
}

impl Drains {
    /// Hand every queued record to the writer or stderr, true when anything was.
    pub(crate) fn sweep(&mut self) -> bool {
        self.sweep_up_to(false)
    }

    /// `sweep`, at most a ring's worth from each drain, so one busy run cannot hold the drain
    /// thread from the others.
    fn sweep_turn(&mut self) -> bool {
        self.sweep_up_to(true)
    }

    fn sweep_up_to(&mut self, capped: bool) -> bool {
        while let Some(drain) = self.joining.try_pop() {
            self.held.push(drain);
        }
        if self.held.is_empty() {
            return false;
        }
        let Drains { held, writer, styled, .. } = self;
        if let Some(writer) = writer {
            return drain_into(held, capped, |rec| writer.record(rec));
        }
        // One write per sweep, not one per format piece.
        let mut lock = io::BufWriter::with_capacity(1 << 14, io::stderr().lock());
        let progressed = drain_into(held, capped, |rec| {
            let _ = format_record_styled(&mut lock, rec, *styled);
        });
        let _ = lock.flush();
        progressed
    }
}

fn drain_into(held: &mut Vec<LogDrain>, capped: bool, mut emit: impl FnMut(&LogRecord)) -> bool {
    let mut progressed = false;
    held.retain_mut(|d| {
        let closed = d.is_closed();
        // A closed ring holds at most its capacity, so a capped turn still empties it.
        let most = if capped { d.rx.capacity() } else { usize::MAX };
        progressed |= d.sweep_up_to(most, &mut emit);
        !closed
    });
    progressed
}

/// A run's drains, shared by the drain thread and the run's end. `None` once the end took them.
pub(crate) type Lane = Arc<Mutex<Option<Drains>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sweep a run's drains on the calling thread, a stepped run's pass.
pub(crate) fn sweep_lane(lane: &Lane) {
    if let Some(drains) = lock(lane).as_mut() {
        drains.sweep();
    }
}

/// Take a run's drains back from the drain thread and sweep what is left. Waits out a sweep in
/// progress, so the writer has every record before the caller writes after them.
pub(crate) fn close_lane(lane: &Lane) {
    let taken = lock(lane).take();
    if let Some(mut drains) = taken {
        drains.sweep();
    }
}

/// The one log drain thread of the process, serving every threaded run.
pub(crate) struct LogHub {
    waiting: Mutex<Waiting>,
    #[cfg(test)]
    turns: AtomicU64,
}

struct Waiting {
    thread: Option<Thread>,
    lanes: Vec<Lane>,
}

pub(crate) static LOG_DRAIN: LogHub = LogHub::new();

impl LogHub {
    // A const empty list, no allocation.
    #[allow(clippy::disallowed_methods)]
    pub(crate) const fn new() -> LogHub {
        LogHub {
            waiting: Mutex::new(Waiting { thread: None, lanes: Vec::new() }),
            #[cfg(test)]
            turns: AtomicU64::new(0),
        }
    }

    /// Hand a run's drains to the drain thread, started here at the first run that logs.
    pub(crate) fn register(&'static self, lane: &Lane) -> io::Result<()> {
        let mut waiting = lock(&self.waiting);
        if waiting.thread.is_none() {
            let builder = std::thread::Builder::new().name("pf-log-drain".into());
            waiting.thread = Some(builder.spawn(|| self.run())?.thread().clone());
        }
        waiting.lanes.push(Arc::clone(lane));
        if let Some(thread) = &waiting.thread {
            thread.unpark();
        }
        Ok(())
    }

    // The process drain thread, not an element.
    #[allow(clippy::disallowed_methods)]
    fn run(&self) {
        let (first, most) = (Duration::from_millis(2), Duration::from_millis(64));
        let (mut idle, mut lanes, mut joining) = (first, Vec::new(), Vec::new());
        loop {
            #[cfg(test)]
            self.turns.fetch_add(1, Relaxed);
            std::mem::swap(&mut lock(&self.waiting).lanes, &mut joining);
            lanes.append(&mut joining);
            if lanes.is_empty() {
                // `register` unparks, so a run that comes while this checks is not missed.
                std::thread::park();
                idle = first;
                continue;
            }
            let mut progressed = false;
            lanes.retain(|lane| sweep_held(lane, &mut progressed));
            match progressed {
                true => idle = first,
                false => {
                    std::thread::park_timeout(idle);
                    idle = (idle * 2).min(most);
                }
            }
        }
    }
}

/// False once the run's end took the drains, or its writer panicked.
fn sweep_held(lane: &Lane, progressed: &mut bool) -> bool {
    let mut held = lock(lane);
    let Some(drains) = held.as_mut() else {
        return false;
    };
    match std::panic::catch_unwind(AssertUnwindSafe(|| drains.sweep_turn())) {
        Ok(swept) => {
            *progressed |= swept;
            true
        }
        Err(_) => {
            // That run loses its records, the other runs keep their drain.
            *held = None;
            false
        }
    }
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn level_ranks_round_trip() {
        for l in [Level::Error, Level::Warn, Level::Info, Level::Debug, Level::Trace] {
            assert_eq!(Level::from_rank(l.rank()), Some(l));
        }
        assert_eq!(Level::from_rank(0), None);
        assert_eq!(Level::from_name("WARN"), Some(Level::Warn));
        assert_eq!(Level::from_name("nonsense"), None);
    }

    #[test]
    fn filter_thresholds_are_inclusive_downward() {
        let f = LevelFilter::new();
        assert!(!f.enabled(Level::Error));
        f.set(Some(Level::Info));
        assert!(f.enabled(Level::Error));
        assert!(f.enabled(Level::Info));
        assert!(!f.enabled(Level::Debug));
        assert!(!f.enabled(Level::Trace));
        f.set(Some(Level::Trace));
        assert!(f.enabled(Level::Trace));
        f.set(None);
        assert!(!f.enabled(Level::Error));
    }

    #[test]
    fn debug_spec_parsing() {
        assert_eq!(DebugSpec::parse("3").global, Some(3));
        assert_eq!(DebugSpec::parse("info").global, Some(Level::Info.rank()));
        assert_eq!(DebugSpec::parse("off").global, Some(0));

        let s = DebugSpec::parse(" filesrc:debug , 2 , http:trace ");
        assert_eq!(s.global, Some(2));
        assert_eq!(
            s.targets,
            vec![
                ("filesrc".to_string(), Level::Debug.rank()),
                ("http".to_string(), Level::Trace.rank()),
            ]
        );

        let s = DebugSpec::parse("garbage:nonsense,,7");
        assert_eq!(s.global, None);
        assert!(s.targets.is_empty());
    }

    #[test]
    fn field_value_conversions() {
        assert!(matches!(FieldValue::from(-5i32), FieldValue::Int(-5)));
        assert!(matches!(FieldValue::from(5u64), FieldValue::Uint(5)));
        assert!(matches!(FieldValue::from(7usize), FieldValue::Uint(7)));
        assert!(matches!(FieldValue::from(true), FieldValue::Bool(true)));
        assert!(matches!(FieldValue::from("x"), FieldValue::Str("x")));
        assert!(matches!(FieldValue::from(ElementId(9)), FieldValue::Uint(9)));
    }

    #[test]
    fn format_line_is_greppable() {
        let mut rec = LogRecord {
            ts: Timestamp::from_millis(12),
            session: 0,
            element: ElementId(3),
            name: "flacdec",
            level: Level::Info,
            event: "header_parsed",
            nfields: 2,
            fields: [
                Field::new("len", FieldValue::Uint(1024)),
                Field::new("codec", FieldValue::Str("flac")),
                EMPTY_FIELD,
                EMPTY_FIELD,
            ],
            text: Text::EMPTY,
        };
        let mut buf = Vec::new();
        format_record(&mut buf, &rec).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("INFO"), "{s}");
        assert!(s.contains("flacdec#3"), "{s}");
        assert!(s.contains("header_parsed"), "{s}");
        assert!(s.contains("len=1024"), "{s}");
        assert!(s.contains("codec=flac"), "{s}");

        rec.name = "";
        let mut buf = Vec::new();
        format_record(&mut buf, &rec).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("elem#3"), "{s}");
    }

    #[test]
    fn disabled_level_is_silent_and_does_not_evaluate_args() {
        let filter = Arc::new(LevelFilter::new());
        let (sink, drain) = log_channel(64);
        let log = Log::new(sink, filter);

        let mut calls = 0u32;
        let mut arg = || {
            calls += 1;
            42u64
        };
        log!(&log, Level::Error, "suppressed", v = arg());
        assert_eq!(calls, 0, "arguments must not be evaluated when the gate is closed");
        assert!(drain.try_next().is_none(), "nothing emitted");
    }

    #[test]
    fn enabled_level_emits_a_structured_record() {
        let filter = Arc::new(LevelFilter::with_level(Level::Warn));
        let (sink, drain) = log_channel(64);
        let mut log = Log::new(sink, filter);
        log.set_element(ElementId(2), "testsink");

        log!(&log, Level::Debug, "below", x = 1);
        log!(&log, Level::Error, "boom", code = 500u32, retry = true);

        let rec = drain.try_next().expect("error record present");
        assert_eq!(rec.event, "boom");
        assert_eq!(rec.element, ElementId(2));
        assert_eq!(rec.level, Level::Error);
        assert_eq!(rec.fields().len(), 2);
        assert_eq!(rec.fields()[0].key, "code");
        assert!(matches!(rec.fields()[0].val, FieldValue::Uint(500)));
        assert!(matches!(rec.fields()[1].val, FieldValue::Bool(true)));
        assert!(drain.try_next().is_none(), "debug record was suppressed");
    }

    #[test]
    fn records_round_trip_through_the_ring_in_order() {
        let filter = Arc::new(LevelFilter::with_level(Level::Trace));
        let (sink, drain) = log_channel(1024);
        let mut log = Log::new(sink, filter);
        log.set_element(ElementId(7), "testsrc");

        let n = 1000u64;
        let producer = std::thread::spawn(move || {
            for i in 0..n {
                log!(&log, Level::Info, "tick", i = i, half = i / 2);
            }
        });

        let mut got = Vec::new();
        while let Some(rec) = drain.next_blocking() {
            got.push(rec);
        }
        producer.join().unwrap();

        assert_eq!(got.len() as u64, n, "every record arrived exactly once");
        for (i, rec) in got.iter().enumerate() {
            assert_eq!(rec.event, "tick");
            assert_eq!(rec.element, ElementId(7));
            assert!(matches!(rec.fields()[0].val, FieldValue::Uint(v) if v == i as u64));
            assert!(matches!(rec.fields()[1].val, FieldValue::Uint(v) if v == i as u64 / 2));
        }
    }

    #[test]
    fn full_ring_drops_and_counts_every_lost_record() {
        let filter = Arc::new(LevelFilter::with_level(Level::Error));
        let (sink, drain) = log_channel(4);
        let log = Log::new(sink, filter);

        let emitted = 100u64;
        for i in 0..emitted {
            log!(&log, Level::Error, "x", i = i);
        }

        let dropped = log.dropped();
        assert!(dropped > 0, "a 4-slot ring must drop under a 100-record burst");

        let mut kept = 0u64;
        while drain.try_next().is_some() {
            kept += 1;
        }
        assert_eq!(kept + dropped, emitted, "kept + dropped == emitted (no record vanishes)");
    }

    #[test]
    fn a_sweep_says_how_many_records_the_ring_dropped_since_the_last_notice() {
        let filter = Arc::new(LevelFilter::with_level(Level::Debug));
        let (sink, mut drain) = log_channel(4);
        let mut log = Log::new(sink, filter);
        log.set_element(ElementId(3), "demux");
        log.set_session(9);
        let swept = |drain: &mut LogDrain| {
            let mut got = Vec::new();
            drain.sweep(|rec| got.push(*rec));
            got
        };
        let notice = |got: &[LogRecord]| {
            got.iter().find(|r| r.event == "log.dropped").map(|r| match r.fields()[0].val {
                FieldValue::Uint(n) => (n, r.level, r.element, r.name, r.session),
                _ => unreachable!(),
            })
        };

        assert!(swept(&mut drain).is_empty(), "nothing logged, nothing said");
        for i in 0..10u64 {
            log!(&log, Level::Debug, "tick", i = i);
        }
        let got = swept(&mut drain);
        assert_eq!(got.len(), 5, "four kept then the notice");
        assert_eq!(got[4].event, "log.dropped", "the notice comes after what was kept");
        assert_eq!(notice(&got), Some((6, Level::Warn, ElementId(3), "demux", 9)));

        log!(&log, Level::Debug, "tick", i = 10u64);
        let got = swept(&mut drain);
        assert_eq!((got.len(), notice(&got)), (1, None), "no new drops, no notice");

        for i in 0..7u64 {
            log!(&log, Level::Debug, "tick", i = i);
        }
        let got = swept(&mut drain);
        assert_eq!(notice(&got).map(|n| n.0), Some(3), "only the drops since the last notice");
        assert_eq!(log.dropped(), 9, "the counter itself keeps the total");
    }

    #[test]
    fn text_is_cut_at_the_cap_on_a_character_boundary() {
        let short = Text::of(format_args!("st={} out={:?}", "ok", Some(3)));
        assert_eq!(short.as_str(), "st=ok out=Some(3)");
        assert!(!short.is_empty() && Text::EMPTY.is_empty());
        let long = Text::of(format_args!("{}", "\u{e9}".repeat(TEXT_CAP)));
        assert_eq!(long.as_str().len(), TEXT_CAP, "two bytes a character, the cap is even");
        assert!(long.as_str().chars().all(|c| c == '\u{e9}'));
        let odd = Text::of(format_args!("a{}", "\u{e9}".repeat(TEXT_CAP)));
        assert_eq!(odd.as_str().len(), TEXT_CAP - 1, "a split character is left out");
        assert_eq!(odd.as_str().chars().count(), TEXT_CAP / 2);
    }

    #[test]
    fn a_record_carries_its_text_after_the_fields() {
        let filter = Arc::new(LevelFilter::with_level(Level::Trace));
        let (sink, drain) = log_channel(64);
        let mut log = Log::new(sink, filter);
        log.set_element(ElementId(4), "vplenc");

        log!(&log, Level::Trace, "query", step = 2u32; "st={} async={}", "MFX_ERR_NONE", 1);
        log!(&log, Level::Trace, "plain", step = 3u32);

        let rec = drain.try_next().expect("the text record");
        assert_eq!((rec.event, rec.fields().len()), ("query", 1));
        assert_eq!(rec.text.as_str(), "st=MFX_ERR_NONE async=1");
        let mut buf = Vec::new();
        format_record(&mut buf, &rec).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.ends_with("vplenc#4 query step=2 st=MFX_ERR_NONE async=1\n"), "{s}");

        let rec = drain.try_next().expect("the plain record");
        assert!(rec.text.is_empty());
        let mut buf = Vec::new();
        format_record(&mut buf, &rec).unwrap();
        assert!(String::from_utf8(buf).unwrap().ends_with("plain step=3\n"));
    }

    #[test]
    fn a_float_field_and_a_session_reach_the_line() {
        let filter = Arc::new(LevelFilter::with_level(Level::Info));
        let (sink, drain) = log_channel(64);
        let mut log = Log::new(sink, filter);
        log.set_element(ElementId(4), "adaptivesrc");
        log.set_session(17);

        log!(&log, Level::Info, "rung", kbps = 2437.5f64, ratio = 0.25f32);

        let rec = drain.try_next().expect("the record");
        assert_eq!(rec.session, 17);
        assert!(matches!(rec.fields()[0].val, FieldValue::Float(v) if v == 2437.5));
        let mut buf = Vec::new();
        format_record(&mut buf, &rec).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.ends_with("adaptivesrc#4@17 rung kbps=2437.5 ratio=0.25\n"), "{s}");

        let mut none = rec;
        none.session = 0;
        let mut buf = Vec::new();
        format_record(&mut buf, &none).unwrap();
        assert!(String::from_utf8(buf).unwrap().contains(" adaptivesrc#4 rung "), "no @ for zero");
    }

    #[derive(Default)]
    struct Routed {
        got: Mutex<Vec<(u64, &'static str, u64, std::thread::ThreadId)>>,
        panics: bool,
    }

    impl LogWriter for Routed {
        fn record(&self, rec: &LogRecord) {
            assert!(!self.panics, "a writer that fails");
            let n = match rec.fields().first().map(|f| f.val) {
                Some(FieldValue::Uint(n)) => n,
                _ => u64::MAX,
            };
            let at = std::thread::current().id();
            self.got.lock().unwrap().push((rec.session, rec.event, n, at));
        }

        fn line(&self, _: Level, _: fmt::Arguments<'_>) {}
    }

    impl Routed {
        fn len(&self) -> usize {
            self.got.lock().unwrap().len()
        }
    }

    fn lane(session: u64, cap: usize, writer: &Arc<Routed>) -> (Log, Lane) {
        let (sink, drain) = log_channel(cap);
        let mut log = Log::new(sink, Arc::new(LevelFilter::with_level(Level::Trace)));
        log.set_session(session);
        let (_, joining) = spsc(1);
        let writer = Some(Arc::clone(writer) as Arc<dyn LogWriter>);
        let drains = Drains { held: vec![drain], joining, writer, styled: false };
        (log, Arc::new(Mutex::new(Some(drains))))
    }

    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn two_runs_share_the_drain_thread_and_each_writer_gets_its_own_records() {
        static HUB: LogHub = LogHub::new();
        let (a, b) = (Arc::new(Routed::default()), Arc::new(Routed::default()));
        let ((log_a, lane_a), (log_b, lane_b)) = (lane(1, 256, &a), lane(2, 256, &b));
        HUB.register(&lane_a).unwrap();
        HUB.register(&lane_b).unwrap();
        for i in 0..100u64 {
            log!(&log_a, Level::Info, "a", i = i);
            log!(&log_b, Level::Info, "b", i = i);
        }
        wait_for("both writers", || a.len() == 100 && b.len() == 100);
        let (a, b) = (a.got.lock().unwrap(), b.got.lock().unwrap());
        assert!(a.iter().enumerate().all(|(i, r)| (r.0, r.1, r.2) == (1, "a", i as u64)), "{a:?}");
        assert!(b.iter().enumerate().all(|(i, r)| (r.0, r.1, r.2) == (2, "b", i as u64)), "{b:?}");
        let drain = a[0].3;
        assert_ne!(drain, std::thread::current().id());
        assert!(a.iter().chain(b.iter()).all(|r| r.3 == drain), "one thread serves both");
    }

    #[test]
    fn the_end_of_a_run_takes_its_drains_back_and_loses_no_record() {
        static HUB: LogHub = LogHub::new();
        let w = Arc::new(Routed::default());
        let (log, lane) = lane(3, 1 << 14, &w);
        HUB.register(&lane).unwrap();
        let emitted = 10_000u64;
        let log = std::thread::spawn(move || {
            for i in 0..emitted {
                log!(&log, Level::Debug, "tick", i = i);
            }
            log
        })
        .join()
        .unwrap();
        close_lane(&lane);
        assert_eq!(log.dropped(), 0);
        let n = w.len();
        assert_eq!(n as u64, emitted, "every record is with the writer when the end returns");
        assert!(w.got.lock().unwrap().iter().enumerate().all(|(i, r)| r.2 == i as u64), "in order");
        log!(&log, Level::Debug, "late", i = 0u64);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(w.len(), n, "the drain thread let go of the run");
    }

    #[test]
    fn the_drain_thread_parks_without_a_timeout_while_it_holds_no_run() {
        static HUB: LogHub = LogHub::new();
        let w = Arc::new(Routed::default());
        let (log, first) = lane(4, 64, &w);
        HUB.register(&first).unwrap();
        log!(&log, Level::Info, "one", i = 0u64);
        wait_for("the first record", || w.len() == 1);
        close_lane(&first);
        // Polling at 64 ms at most, so a still window of 300 ms is a park.
        let still = (0..20).any(|_| {
            let before = HUB.turns.load(Relaxed);
            std::thread::sleep(Duration::from_millis(300));
            HUB.turns.load(Relaxed) == before
        });
        assert!(still, "the drain thread kept turning with no run");
        let (log, second) = lane(4, 64, &w);
        HUB.register(&second).unwrap();
        log!(&log, Level::Info, "two", i = 1u64);
        wait_for("a run that registers wakes it", || w.len() == 2);
    }

    #[test]
    fn the_drain_thread_still_counts_what_a_full_ring_drops() {
        static HUB: LogHub = LogHub::new();
        let w = Arc::new(Routed::default());
        let (log, lane) = lane(5, 4, &w);
        let emitted = 200u64;
        for i in 0..emitted {
            log!(&log, Level::Info, "x", i = i);
        }
        assert_eq!(log.dropped(), 196, "a 4 slot ring with no drain yet");
        HUB.register(&lane).unwrap();
        let told = || {
            let got = w.got.lock().unwrap();
            let kept = got.iter().filter(|r| r.1 == "x").count() as u64;
            let notices = got.iter().filter(|r| r.1 == "log.dropped").map(|r| r.2).sum::<u64>();
            kept + notices
        };
        wait_for("kept and dropped to add up", || told() == emitted);
        close_lane(&lane);
        assert_eq!(told(), emitted);
    }

    #[test]
    fn a_writer_that_panics_loses_only_its_own_run() {
        static HUB: LogHub = LogHub::new();
        let bad = Arc::new(Routed { panics: true, ..Routed::default() });
        let good = Arc::new(Routed::default());
        let ((log_bad, lane_bad), (log_good, lane_good)) = (lane(6, 64, &bad), lane(7, 64, &good));
        HUB.register(&lane_bad).unwrap();
        log!(&log_bad, Level::Info, "boom", i = 0u64);
        wait_for("the bad run to be dropped", || lane_bad.lock().unwrap().is_none());
        HUB.register(&lane_good).unwrap();
        log!(&log_good, Level::Info, "fine", i = 0u64);
        wait_for("the other run", || good.len() == 1);
        close_lane(&lane_bad);
        close_lane(&lane_good);
    }

    /// A writer whose every record logs another into its own run, so its ring never empties.
    #[derive(Default)]
    struct Echo {
        log: Mutex<Option<Log>>,
        seen: AtomicU64,
    }

    impl LogWriter for Echo {
        fn record(&self, _: &LogRecord) {
            self.seen.fetch_add(1, Relaxed);
            if let Some(log) = self.log.lock().unwrap().as_ref() {
                log!(log, Level::Info, "echo", i = 0u64);
            }
        }

        fn line(&self, _: Level, _: fmt::Arguments<'_>) {}
    }

    #[test]
    fn a_run_that_logs_faster_than_its_writer_leaves_the_drain_to_the_others() {
        static HUB: LogHub = LogHub::new();
        let echo = Arc::new(Echo::default());
        let (sink, drain) = log_channel(64);
        let mut log = Log::new(sink, Arc::new(LevelFilter::with_level(Level::Trace)));
        log.set_session(8);
        log!(&log, Level::Info, "echo", i = 0u64);
        *echo.log.lock().unwrap() = Some(log);
        let (_, joining) = spsc(1);
        let writer = Some(Arc::clone(&echo) as Arc<dyn LogWriter>);
        let drains = Drains { held: vec![drain], joining, writer, styled: false };
        let busy: Lane = Arc::new(Mutex::new(Some(drains)));
        HUB.register(&busy).unwrap();
        wait_for("the echo to run", || echo.seen.load(Relaxed) > 1000);
        let other = Arc::new(Routed::default());
        let (log_other, lane_other) = lane(9, 64, &other);
        HUB.register(&lane_other).unwrap();
        log!(&log_other, Level::Info, "other", i = 0u64);
        wait_for("the other run's record past the busy one", || other.len() == 1);
        // Stops the echo, so the busy run's end finds its ring drained.
        echo.log.lock().unwrap().take();
        close_lane(&busy);
        close_lane(&lane_other);
    }
}
