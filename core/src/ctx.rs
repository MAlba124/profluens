use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::batch::{Batch, Inputs, OutBatch};
use crate::buffer::{Buffer, BufferFlags};
use crate::bus::{BusMessage, BusSender};
use crate::clock::Clock;
use crate::command::{Control, Cursor};
use crate::element::ElementDesc;
use crate::error::{Error, GraphError, OfferFault, Scope};
use crate::event::Event;
use crate::feedback::{Feedback, Upstream};
use crate::format::offer_line;
use crate::format::{
    FixedFormat, Interned, OfferDesc, OfferSpec, Value, ValueDesc, Vocabulary, admit,
};
use crate::id::{ElementId, FieldId, FormatId, SinkPad, SrcPad, ValueId};
use crate::io::{Completion, Io, Mailbox, Submission};
use crate::log::{Field, Level, Log, Loggable, Text};
use crate::memory::Pool;
use crate::pipeline::pad_at;
use crate::props::PropTable;
use crate::pull::{Ask, Mode, Port, RangeError};
use crate::scratch::Arena;
use crate::time::{Rate, Timestamp};
use crate::timeline::clamp;

/// The largest slot `grow_slots` maps, 1 GiB. A frame past it is refused on capacity.
pub const SLOT_MAX: usize = 1 << 30;

pub(crate) struct Announcement {
    pub(crate) pad: SrcPad,
    pub(crate) at: u32,
    pub(crate) payload: AnnouncePayload,
}

// The format rides inline, boxing it would allocate per announce.
#[allow(clippy::large_enum_variant)]
pub(crate) enum AnnouncePayload {
    Named { family: &'static str, fields: Vec<(&'static str, ValueDesc)> },
    Fixed(FixedFormat),
}

pub(crate) struct AddedPad {
    pub(crate) name: String,
    pub(crate) offers: &'static [OfferDesc],
}

#[derive(Clone)]
pub(crate) struct PadOffers {
    pub(crate) key: (bool, u32),
    pub(crate) name: Box<str>,
    pub(crate) offers: Interned,
    pub(crate) lines: Box<[Box<str>]>,
    /// The pipeline's id of these offers, so the solver lowers each menu once. `NONE` for none.
    pub(crate) menu: u32,
}

#[derive(Default)]
pub(crate) struct PadFormats {
    pub(crate) sinks: Vec<Option<FixedFormat>>,
    pub(crate) srcs: Vec<Option<FixedFormat>>,
    pub(crate) offers: Vec<PadOffers>,
    /// Per src pad what every typed link of it takes of each announced field, see
    /// [`Ctx::src_takes`].
    pub(crate) takes: Vec<Option<Takes>>,
    /// Per src pad the menus of the pads it feeds, see [`Ctx::src_peers_prefer`].
    pub(crate) peers: Vec<Vec<PeerMenu>>,
}

pub(crate) type Takes = Vec<(FieldId, Vec<Value>)>;

/// The menu of a pad a src pad feeds, the family each offer names and whether it is the
/// wildcard, in the menu's order.
pub(crate) type PeerMenu = Arc<[(Box<str>, bool)]>;

impl PadFormats {
    // Once per run and per change, at the group tables.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn set_peers(&mut self, pad: SrcPad, menus: Vec<PeerMenu>) {
        let i = pad.0 as usize;
        if self.peers.len() <= i {
            self.peers.resize(i + 1, Vec::new());
        }
        self.peers[i] = menus;
    }

    // Once per run and per change, at the group tables.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn hold_to(&mut self, pad: SrcPad, link: &[(FieldId, Vec<Value>)]) {
        let i = pad.0 as usize;
        if self.takes.len() <= i {
            self.takes.resize(i + 1, None);
        }
        let Some(held) = &mut self.takes[i] else {
            self.takes[i] = Some(link.to_vec());
            return;
        };
        for (field, vals) in link {
            match held.iter_mut().find(|(f, _)| f == field) {
                Some((_, kept)) => kept.retain(|v| vals.contains(v)),
                None => held.push((*field, vals.clone())),
            }
        }
    }
}

/// Why an element stops, as [`Ctx::stop_reason`] says in its `stop`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StopReason {
    /// Its group or the run ended, or the run was stopped.
    RunEnd,
    /// A change that committed removed it, by drain or by cut.
    Removed,
    /// The change that added it failed or was cancelled, and is taken back.
    RolledBack,
}

/// What a stream is, as far as a player sorts them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StreamKind {
    Video,
    Audio,
    Text,
    Other,
}

impl StreamKind {
    /// Whether a stream of this kind is sparse by default, text and data with long gaps.
    pub const fn sparse(self) -> bool {
        matches!(self, StreamKind::Text | StreamKind::Other)
    }
}

/// What a container says a stream is for (RFC 9559 5.1.4.1 track flags, DASH roles).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Disposition {
    /// The container has it play.
    pub enabled: bool,
    pub commentary: bool,
    pub hearing_impaired: bool,
    pub visual_impaired: bool,
    pub descriptions: bool,
    pub original: bool,
}

impl Disposition {
    /// A stream that plays and is for no one in particular.
    pub const MAIN: Disposition = Disposition {
        enabled: true,
        commentary: false,
        hearing_impaired: false,
        visual_impaired: false,
        descriptions: false,
        original: false,
    };
}

impl Default for Disposition {
    fn default() -> Disposition {
        Disposition::MAIN
    }
}

/// One selectable stream of an element, which gets a pad only once selected.
#[derive(Clone, Debug)]
pub struct Stream {
    pub kind: StreamKind,
    pub codec: &'static str,
    /// BCP 47 or as the container states, empty for none.
    pub language: Box<str>,
    pub title: Box<str>,
    pub default: bool,
    pub forced: bool,
    pub disposition: Disposition,
    /// The channels the container states of a sound, 0 where it states none.
    pub channels: u16,
    pub duration: Timestamp,
    /// Long gaps between its buffers, so its links neither measure the interleave nor starve.
    pub sparse: bool,
    pub name: Box<str>,
    pub offers: &'static [OfferDesc],
}

/// Where a source reads behind a seek, the byte and its stream time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SeekTarget {
    pub to_byte: u64,
    pub at: Timestamp,
}

/// An element's view of its input, output, memory, IO and the bus.
pub struct Ctx {
    pool: Pool,
    input: Batch,
    outs: Vec<Batch>,
    added_srcs: Vec<AddedPad>,
    added_sinks: Vec<AddedPad>,
    pub(crate) probe: Option<(&'static ElementDesc, Vocabulary, Vec<PadOffers>)>,
    nsrcs: u32,
    nsinks: u32,
    pad_inputs: Vec<Batch>,
    pad_eos: Vec<bool>,
    bus: BusSender,
    element: ElementId,
    out_format: FormatId,
    spare_outputs: Vec<Batch>,
    spare_cap: usize,
    spare_inputs: Vec<Batch>,
    negotiated: PadFormats,
    credits: u32,
    io: Mailbox,
    log: Option<Log>,
    announced: Vec<Announcement>,
    vocabulary: Option<Arc<Vocabulary>>,
    clock: Option<Arc<dyn Clock>>,
    steady: Option<std::time::Instant>,
    running_base: Arc<std::sync::atomic::AtomicI64>,
    path_latency: Timestamp,
    trace: Option<(Arc<crate::counters::ElementCounters>, Arc<std::sync::atomic::AtomicBool>)>,
    tail: Option<Arc<crate::counters::ElementCounters>>,
    paused: bool,
    offset: i64,
    linked_src_pads: Vec<bool>,
    streams: Vec<Stream>,
    stream_pads: Vec<(u32, SrcPad)>,
    seek_target: Option<SeekTarget>,
    applied_seek_gen: u64,
    present_from: Option<Timestamp>,
    preview: bool,
    still: bool,
    control: Option<(Arc<Control>, Arc<Cursor>)>,
    scratch: Arena,
    props: Option<(&'static [crate::element::PropDesc], Arc<PropTable>)>,
    wake_at: Timestamp,
    wake_within: Duration,
    station: Option<Arc<crate::station::Station>>,
    starved: bool,
    map_failures_seen: u64,
    end_held: bool,
    room_asked: bool,
    full_pads: Vec<bool>,
    /// A track of this element starves behind another in its output.
    behind: bool,
    refused: Vec<bool>,
    event_pad: Option<SinkPad>,
    fb_in: Vec<Upstream>,
    fb_out: Vec<(Upstream, bool)>,
    pub(crate) serving: Port,
    pub(crate) pulling: Port,
    stop_reason: StopReason,
}

/// One flag per pad, grown on a set and never on a clear.
fn set_flag(flags: &mut Vec<bool>, at: usize, on: bool) {
    match flags.get_mut(at) {
        Some(flag) => *flag = on,
        None if on => {
            flags.resize(at + 1, false);
            flags[at] = true;
        }
        None => {}
    }
}

impl Ctx {
    // Once per element at setup, empty Vecs allocate nothing.
    #[allow(clippy::disallowed_methods)]
    pub fn new(
        pool: Pool,
        bus: BusSender,
        element: ElementId,
        out_format: FormatId,
        credits: u32,
    ) -> Self {
        Self {
            input: Batch::new(out_format),
            spare_outputs: Vec::new(),
            spare_cap: 8,
            spare_inputs: Vec::new(),
            outs: Vec::new(),
            added_srcs: Vec::new(),
            added_sinks: Vec::new(),
            probe: None,
            nsrcs: 0,
            nsinks: 0,
            pad_inputs: Vec::new(),
            pad_eos: Vec::new(),
            credits: if credits > 0 { credits } else { pool.io_depth() },
            pool,
            bus,
            element,
            out_format,
            negotiated: PadFormats::default(),
            io: Mailbox::default(),
            log: None,
            announced: Vec::new(),
            vocabulary: None,
            clock: None,
            steady: None,
            running_base: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            seek_target: None,
            applied_seek_gen: 0,
            present_from: None,
            preview: false,
            still: false,
            control: None,
            scratch: Arena::default(),
            props: None,
            path_latency: Timestamp::ZERO,
            trace: None,
            tail: None,
            paused: false,
            offset: 0,
            linked_src_pads: Vec::new(),
            streams: Vec::new(),
            stream_pads: Vec::new(),
            wake_at: Timestamp::NONE,
            wake_within: Duration::MAX,

            station: None,
            starved: false,
            map_failures_seen: 0,
            end_held: false,
            room_asked: false,
            full_pads: Vec::new(),
            behind: false,
            refused: Vec::new(),
            event_pad: None,
            fb_in: Vec::new(),
            fb_out: Vec::new(),
            serving: Port::default(),
            pulling: Port::default(),
            stop_reason: StopReason::RunEnd,
        }
    }

    pub fn element(&self) -> ElementId {
        self.element
    }

    pub(crate) fn set_log(&mut self, log: Log) {
        self.log = Some(log);
    }

    pub fn negotiated_sink(&self, pad: SinkPad) -> Option<&FixedFormat> {
        self.negotiated.sinks.get(pad.0 as usize).and_then(|f| f.as_ref())
    }

    pub fn negotiated_src(&self, pad: SrcPad) -> Option<&FixedFormat> {
        self.negotiated.srcs.get(pad.0 as usize).and_then(|f| f.as_ref())
    }

    /// The values of `field` every consumer downstream of src pad `pad` takes, `None` where any is
    /// taken.
    pub fn src_takes(&self, pad: SrcPad, field: FieldId) -> Option<&[Value]> {
        let held = self.negotiated.takes.get(pad.0 as usize)?.as_ref()?;
        held.iter().find(|(f, _)| *f == field).map(|(_, v)| v.as_slice())
    }

    /// Which of `families` the pads that src pad `pad` feeds name first in their own menus,
    /// where each names one of them and all agree. `None` where one takes any family or names
    /// none of them, where they disagree and where the pad feeds nothing.
    ///
    /// A link is made in the source's menu order, so a source that can put one stream out in
    /// several families reads its consumers' order here and announces by it.
    pub fn src_peers_prefer<'f>(&self, pad: SrcPad, families: &[&'f str]) -> Option<&'f str> {
        let menus = self.negotiated.peers.get(pad.0 as usize)?;
        let named = |menu: &PeerMenu| {
            let typed = menu.iter().all(|(_, wildcard)| !wildcard);
            let first = menu.iter().find_map(|(f, _)| families.iter().find(|x| ***x == **f));
            first.copied().filter(|_| typed)
        };
        let first = named(menus.first()?)?;
        menus[1..].iter().all(|menu| named(menu) == Some(first)).then_some(first)
    }

    /// Record the menus of the pads src pad `pad` feeds, for a harness.
    // Test setup, once per pad.
    #[allow(clippy::disallowed_methods)]
    pub fn set_src_peers(&mut self, pad: SrcPad, menus: &[&[OfferSpec<'_>]]) {
        let menu = |offers: &&[OfferSpec<'_>]| -> PeerMenu {
            offers.iter().map(|o| (o.family.into(), o.is_wildcard())).collect()
        };
        self.negotiated.set_peers(pad, menus.iter().map(menu).collect());
    }

    pub(crate) fn feeds(&mut self, pad: SrcPad, menus: Vec<PeerMenu>) {
        self.negotiated.set_peers(pad, menus);
    }

    /// Record what src pad `pad`'s consumers take of `field`, for a harness.
    // Test setup, once per field.
    #[allow(clippy::disallowed_methods)]
    pub fn set_src_takes(&mut self, pad: SrcPad, field: FieldId, values: &[Value]) {
        self.negotiated.hold_to(pad, &[(field, values.to_vec())]);
    }

    pub(crate) fn set_negotiated(&mut self, formats: PadFormats) {
        self.negotiated = formats;
    }

    pub(crate) fn sink_offers(&self, pad: SinkPad) -> Option<&PadOffers> {
        self.negotiated.offers.iter().find(|o| o.key == (false, pad.0))
    }

    /// Announces a runtime output format on `pad`, sent on only when it differs.
    // Once per format change, not per buffer.
    #[allow(clippy::disallowed_methods)]
    pub fn announce_format(
        &mut self,
        pad: SrcPad,
        family: &'static str,
        fields: &[(&'static str, ValueDesc)],
    ) {
        let at = self.out_slot(pad).len() as u32;
        self.announced.push(Announcement {
            pad,
            at,
            payload: AnnouncePayload::Named { family, fields: fields.to_vec() },
        });
    }

    /// Re-announces an already resolved format on `pad`, for pure transports.
    pub fn forward_format(&mut self, pad: SrcPad, format: FixedFormat) {
        let at = self.out_slot(pad).len() as u32;
        self.announced.push(Announcement { pad, at, payload: AnnouncePayload::Fixed(format) });
    }

    pub(crate) fn take_announcements(&mut self) -> Vec<Announcement> {
        std::mem::take(&mut self.announced)
    }

    pub fn set_negotiated_sink(&mut self, pad: SinkPad, format: FixedFormat) {
        set_slot(&mut self.negotiated.sinks, pad.0, format);
    }

    pub fn set_negotiated_src(&mut self, pad: SrcPad, format: FixedFormat) {
        set_slot(&mut self.negotiated.srcs, pad.0, format);
    }

    pub fn set_vocabulary(&mut self, vocabulary: Arc<Vocabulary>) {
        self.vocabulary = Some(vocabulary);
    }

    pub fn set_clock(&mut self, clock: Arc<dyn Clock>, base: Arc<std::sync::atomic::AtomicI64>) {
        self.clock = Some(clock);
        self.running_base = base;
    }

    pub(crate) fn set_steady(&mut self, epoch: Option<std::time::Instant>) {
        self.steady = epoch;
    }

    /// Nanoseconds that no pause stops and no seek moves.
    pub fn steady(&self) -> u64 {
        match (self.steady, &self.clock) {
            (Some(epoch), _) => epoch.elapsed().as_nanos() as u64,
            (None, Some(clock)) => clock.now().nanos().unwrap_or(0),
            (None, None) => 0,
        }
    }

    pub(crate) fn set_linked_pads(&mut self, linked: Vec<bool>) {
        self.linked_src_pads = linked;
    }

    /// The src pad that feeds the next member of a group inline, the one linked pad of a member
    /// that is not the group's tail, which may be any of an element's pads, a demuxer's audio.
    pub(crate) fn hop_pad(&self) -> SrcPad {
        SrcPad(self.linked_src_pads.iter().position(|on| *on).unwrap_or(0) as u32)
    }

    pub(crate) fn set_pad_linked(&mut self, pad: SrcPad, on: bool) {
        let i = pad.0 as usize;
        if self.linked_src_pads.len() <= i {
            self.linked_src_pads.resize(i + 1, false);
        }
        self.linked_src_pads[i] = on;
    }

    /// Describes a stream from `preroll` and returns its number in the collection.
    pub fn add_stream(&mut self, stream: Stream) -> u32 {
        self.streams.push(stream);
        self.streams.len() as u32 - 1
    }

    pub fn take_streams(&mut self) -> Vec<Stream> {
        std::mem::take(&mut self.streams)
    }

    /// The src pad stream `stream` goes out on, `None` while unselected.
    pub fn stream_pad(&self, stream: u32) -> Option<SrcPad> {
        self.stream_pads.iter().find(|(s, _)| *s == stream).map(|(_, pad)| *pad)
    }

    pub fn set_stream_pad(&mut self, stream: u32, pad: SrcPad) {
        self.stream_pads.retain(|(s, _)| *s != stream);
        self.stream_pads.push((stream, pad));
    }

    /// Whether `pad` has a linked downstream edge, `true` outside a run.
    pub fn pad_linked(&self, pad: SrcPad) -> bool {
        self.linked_src_pads.get(pad.0 as usize).copied().unwrap_or(true)
    }

    /// The negotiated output `FormatId` pool buffers are stamped with.
    pub fn out_format(&self) -> FormatId {
        self.out_format
    }

    /// Why the element stops, read in its `stop`. `RunEnd` before.
    pub fn stop_reason(&self) -> StopReason {
        self.stop_reason
    }

    pub(crate) fn set_stop_reason(&mut self, reason: StopReason) {
        self.stop_reason = reason;
    }

    pub(crate) fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub(crate) fn set_offset(&mut self, by: i64) {
        self.offset = by;
    }

    pub fn offset(&self) -> i64 {
        self.offset
    }

    fn base_raw(&self) -> i64 {
        let raw = self.running_base.load(std::sync::atomic::Ordering::Acquire);
        if raw == crate::time::BASE_UNSET { 0 } else { raw }
    }

    pub(crate) fn set_path_latency(&mut self, latency: Timestamp) {
        self.path_latency = latency;
    }

    pub(crate) fn set_trace(
        &mut self,
        counters: Arc<crate::counters::ElementCounters>,
        gate: Arc<std::sync::atomic::AtomicBool>,
    ) {
        self.trace = Some((counters, gate));
    }

    /// This element's upstream path latency, already folded into [`due`](Self::due).
    pub fn path_latency(&self) -> Timestamp {
        self.path_latency
    }

    pub(crate) fn set_control(
        &mut self,
        control: Arc<Control>,
        cursor: Arc<Cursor>,
        seek_gen: u64,
    ) {
        (self.control, self.applied_seek_gen) = (Some((control, cursor)), seek_gen);
    }

    pub fn set_seek_target(
        &mut self,
        seek_gen: u64,
        to_time: Timestamp,
        target: Option<SeekTarget>,
    ) {
        (self.applied_seek_gen, self.seek_target) = (seek_gen, target);
        self.set_presentation(to_time);
    }

    pub(crate) fn aim(&mut self, target: Option<SeekTarget>) {
        self.seek_target = target;
    }

    pub fn set_presentation(&mut self, to_time: Timestamp) {
        self.present_from = to_time.is_some().then_some(to_time);
        self.preview = false;
    }

    /// The seek just applied wants a preview, see [`take_preview`](Self::take_preview).
    pub fn arm_preview(&mut self) {
        self.preview = self.present_from.is_some();
    }

    pub(crate) fn set_still(&mut self, still: bool) {
        self.still = still;
    }

    /// Whether the seek last applied shows its keyframe alone, [`SeekMode::Key`]. A decoder
    /// decodes nothing behind that picture, the next seek drops it.
    ///
    /// [`SeekMode::Key`]: crate::pipeline::SeekMode::Key
    pub fn still(&self) -> bool {
        self.still
    }

    /// Whether a decoder should let out the picture it drops before the presentation point as a
    /// preview.
    pub fn take_preview(&mut self) -> bool {
        std::mem::take(&mut self.preview)
    }

    pub fn presentation_point(&self) -> Option<Timestamp> {
        self.present_from
    }

    /// Whether a buffer lies wholly before the presentation point.
    pub fn before_presentation(&self, pts: Timestamp, duration: Timestamp) -> bool {
        self.present_from.is_some_and(|at| pts < at && pts.saturating_add(duration) <= at)
    }

    pub fn count_drop(&self) {
        self.trace.iter().for_each(|(counters, _)| counters.record_drops(1));
    }

    /// The buffers the last element of this element's group put out so far, None outside a
    /// run. A decoder holds its next decode on it until the picture a seek waits for is out.
    pub fn group_out(&self) -> Option<u64> {
        self.tail.as_ref().map(|c| c.buffers_out())
    }

    pub(crate) fn set_tail(&mut self, counters: Arc<crate::counters::ElementCounters>) {
        self.tail = Some(counters);
    }

    /// Whether the app appended a command this group has yet to apply.
    pub fn command_pending(&self) -> bool {
        self.control.as_ref().is_some_and(|(control, cursor)| control.log.head() != cursor.get())
    }

    /// The newest seek generation the app issued, which may run ahead of the group.
    pub fn seek_gen(&self) -> u64 {
        self.control.as_ref().map_or(0, |(control, _)| control.seek_gen())
    }

    pub fn applied_seek_gen(&self) -> u64 {
        self.applied_seek_gen
    }

    /// Where this element reads behind the seek its group is flushing for.
    pub fn seek_target(&self) -> Option<SeekTarget> {
        self.seek_target
    }

    pub fn field_id(&self, name: &str) -> Option<FieldId> {
        self.vocabulary.as_ref()?.field_id(name)
    }

    pub fn value_id(&self, name: &str) -> Option<ValueId> {
        self.vocabulary.as_ref()?.value_id(name)
    }

    pub fn family_name(&self, id: FormatId) -> Option<&str> {
        self.vocabulary.as_ref()?.family_name(id)
    }

    pub fn family_id(&self, name: &str) -> Option<FormatId> {
        self.vocabulary.as_ref()?.family_id(name)
    }

    pub fn value_name(&self, id: ValueId) -> Option<&str> {
        self.vocabulary.as_ref()?.value_name(id)
    }

    pub fn alloc(&mut self) -> Buffer {
        self.buffer(self.pool.acquire())
    }

    /// Allocates within the pool cap, `None` is the backpressure signal.
    pub fn try_alloc(&mut self) -> Option<Buffer> {
        let memory = match &self.station {
            Some(station) => self.pool.try_acquire_or_enlist(station),
            None => self.pool.try_acquire(),
        };
        if memory.is_none() {
            self.starved = true;
            // A region the allocator refused is no backpressure, nothing frees it.
            let failures = self.pool.map_failures();
            if failures > self.map_failures_seen {
                self.map_failures_seen = failures;
                let slot_size = self.pool.slot_size();
                crate::log!(&*self, Level::Error, "map_failed", slot_size = slot_size);
            }
        }
        Some(self.buffer(memory?))
    }

    /// A buffer of at least `n` bytes within the pool budgets, `Ok(None)` is backpressure.
    pub fn try_alloc_sized(&mut self, n: usize) -> Result<Option<Buffer>, crate::memory::TooLarge> {
        let memory = match &self.station {
            Some(station) => self.pool.try_acquire_sized_or_enlist(n, station)?,
            None => self.pool.try_acquire_sized(n)?,
        };
        if memory.is_none() {
            self.starved = true;
        }
        Ok(memory.map(|m| self.buffer(m)))
    }

    /// Whether the ring src pad `pad` feeds has room for a batch.
    pub fn room(&mut self, pad: SrcPad) -> bool {
        self.room_asked = true;
        !self.full_pads.get(pad.0 as usize).copied().unwrap_or(false)
    }

    pub fn event_pad(&self) -> Option<SinkPad> {
        self.event_pad
    }

    pub(crate) fn set_event_pad(&mut self, pad: Option<SinkPad>) {
        self.event_pad = pad;
    }

    pub(crate) fn asks_room(&self) -> bool {
        self.room_asked
    }

    pub(crate) fn clear_full_pads(&mut self) {
        self.full_pads.iter_mut().for_each(|full| *full = false);
    }

    pub(crate) fn set_full_pad(&mut self, pad: SrcPad) {
        set_flag(&mut self.full_pads, pad.0 as usize, true);
    }

    /// Whether a track this element puts out starves behind another in its input.
    pub fn behind(&self) -> bool {
        self.behind
    }

    pub(crate) fn set_behind(&mut self, behind: bool) {
        self.behind = behind;
    }

    /// Publishes this element's time to byte map, replacing the last. A seek held for it is sent.
    pub fn publish_index(&self, index: crate::pipeline::Index) {
        if let Some((counters, _)) = &self.trace {
            counters.index.publish(index);
            counters.set_indexing(false);
            counters.set_relinked(false);
            self.resolve_pending();
        }
        self.bus.send(BusMessage::IndexPublished { element: self.element });
    }

    /// The element saw its links change. Planning on its index goes on, republished or not,
    /// and a seek held for it is sent.
    pub(crate) fn link_seen(&self) {
        if let Some((counters, _)) = &self.trace {
            counters.set_relinked(false);
            self.resolve_pending();
        }
    }

    /// Says how this source is addressed, so a seek knows what it can ask of it.
    pub fn publish_serves(&self, serves: crate::pipeline::Serves) {
        if let Some((counters, _)) = &self.trace {
            counters.set_serves(serves);
            self.resolve_pending();
        }
    }

    /// Says this element publishes an index once it has read the head, so an early seek is held.
    pub fn will_index(&self) {
        if let Some((counters, _)) = &self.trace {
            counters.set_indexing(true);
        }
    }

    pub fn set_duration(&mut self, ns: u64) {
        if let Some((counters, _)) = &self.trace {
            counters.set_duration(ns);
        }
        let element = self.element;
        self.post(BusMessage::DurationChanged { element, ns });
    }

    fn resolve_pending(&self) {
        if let Some((control, _)) = &self.control {
            control.resolve_pending();
        }
    }

    /// The gauge and ranges of the store this element keeps ahead of its reader.
    pub fn store(&self) -> Option<(&crate::level::Gauge, &crate::level::Ranges)> {
        self.trace.as_ref().map(|(counters, _)| (&counters.store, &counters.ranges))
    }

    /// Says the octets a second this source's input comes in at, for a tap.
    pub fn publish_in_rate(&self, bytes_per_second: u64) {
        if let Some((counters, _)) = &self.trace {
            counters.set_in_rate(bytes_per_second);
        }
    }

    /// Posts a buffering message if the store crossed a mark since last asked.
    pub fn post_fill(&mut self) {
        let crossed = self.store().and_then(|(store, _)| Some((store.crossed()?, store.held())));
        if let Some((fill, held)) = crossed {
            self.post(BusMessage::Buffering { element: self.element, pad: None, fill, held });
        }
    }

    /// Wakes the group when a pool slot returns, the caller looks once more after this.
    pub fn await_recycle(&mut self) {
        if let Some(station) = &self.station {
            self.pool.enlist(station);
        }
        self.starved = true;
    }

    /// Holds the end of the stream for this pass while output remains.
    pub fn hold_end(&mut self) {
        self.end_held = true;
    }

    pub fn take_end_held(&mut self) -> bool {
        std::mem::take(&mut self.end_held)
    }

    pub(crate) fn set_station(&mut self, station: Arc<crate::station::Station>) {
        self.station = Some(station);
    }

    pub(crate) fn take_starved(&mut self) -> bool {
        std::mem::take(&mut self.starved)
    }

    pub fn pool_stats(&self) -> crate::memory::PoolStats {
        self.pool.stats()
    }

    /// Keeps `count` buffers of the class serving `size` mapped for the run.
    pub fn reserve_sized(
        &self,
        size: usize,
        count: u32,
    ) -> Result<(), crate::memory::ReserveError> {
        self.pool.reserve_sized(size, count)
    }

    pub fn pool_id(&self) -> crate::memory::PoolId {
        self.pool.id()
    }

    /// A buffer of at least `n` bytes for cold paths, not zeroed. `None` past
    /// [`crate::memory::EXACT_MAX`] or when the heap refuses it.
    pub fn alloc_exact(&mut self, n: usize) -> Option<Buffer> {
        self.pool.acquire_exact(n).map(|m| self.buffer(m))
    }

    /// The failure of an exact buffer of `n` bytes refused, scoped to the buffer so the element
    /// drops what needed it and goes on.
    pub fn exact_refused(&self, n: usize) -> Error {
        let max = crate::memory::EXACT_MAX;
        Error::element_failed(self.element(), crate::memory::TooLarge { n, max })
            .scoped(Scope::Buffer)
    }

    /// A buffer over `memory` the element already holds, stamped with the src format.
    pub fn buffer_of(&self, memory: crate::memory::Memory) -> Buffer {
        self.buffer(memory)
    }

    pub fn slot_size(&self) -> usize {
        self.pool.slot_size()
    }

    /// Grows this element's pool slots to `n` bytes when smaller, `true` when it grew. Past
    /// [`SLOT_MAX`] nothing grows, and the next acquire fails on capacity instead of stalling.
    pub fn grow_slots(&mut self, n: usize) -> bool {
        let grow = self.pool.slot_size() < n && n <= SLOT_MAX;
        if grow {
            self.pool.resize(n);
        }
        grow
    }

    /// Raises this element's pool cap to `slots` when smaller, `true` when it rose.
    pub fn grow_slot_cap(&mut self, slots: u32) -> bool {
        self.pool.grow_cap(slots)
    }

    fn buffer(&self, memory: crate::memory::Memory) -> Buffer {
        Buffer {
            memory,
            pts: Timestamp::NONE,
            dts: Timestamp::NONE,
            duration: Timestamp::NONE,
            flags: BufferFlags::empty(),
            format: self.out_format,
            sync: None,
        }
    }

    pub fn out(&mut self, pad: SrcPad) -> OutBatch<'_> {
        OutBatch::new(self.out_slot(pad))
    }

    pub fn io(&mut self) -> Io<'_> {
        Io::new(self.element, &mut self.io, self.credits)
    }

    /// Offers range reads on src pad `pad`, from `start`.
    pub fn offer_ranges(&mut self, pad: SrcPad) {
        if pad.0 == 0 {
            self.serving.mode = Mode::Offered;
        }
    }

    /// Takes the offer back, from `process` before any range is served.
    pub fn withdraw_ranges(&mut self, pad: SrcPad) {
        if pad.0 == 0 && self.serving.mode != Mode::Off {
            self.serving.mode = Mode::Withdrawn;
        }
    }

    /// Whether the consumer of src pad `pad` pulls ranges and takes nothing else.
    pub fn pulled(&self, pad: SrcPad) -> bool {
        pad.0 == 0 && self.serving.is_on()
    }

    /// The next range asked of src pad `pad`, in the order asked.
    pub fn next_ask(&mut self, pad: SrcPad) -> Option<Ask> {
        if !self.pulled(pad) {
            return None;
        }
        self.serving.asks.pop_front()
    }

    /// Whether the consumer asked its last range and every one was taken.
    pub fn asks_over(&self, pad: SrcPad) -> bool {
        self.pulled(pad) && self.serving.over && self.serving.asks.is_empty()
    }

    /// Puts out the buffer of the range tagged `tag`, in any order.
    pub fn serve(&mut self, pad: SrcPad, tag: u64, buf: Buffer) {
        if !self.pulled(pad) {
            return;
        }
        self.out_slot(pad).push(buf);
        self.serving.tags.push_back(tag);
    }

    /// Asks to pull ranges on sink pad `pad`, from `start`.
    pub fn want_ranges(&mut self, pad: SinkPad) {
        if pad.0 == 0 {
            self.pulling.mode = Mode::Wanted;
        }
    }

    /// Whether sink pad `pad` pulls ranges from its source.
    pub fn pulls(&self, pad: SinkPad) -> bool {
        pad.0 == 0 && self.pulling.is_on()
    }

    /// Ranges sink pad `pad` may still ask before one is served.
    pub fn asks_left(&self, pad: SinkPad) -> u32 {
        if !self.pulls(pad) {
            return 0;
        }
        self.credits.saturating_sub(self.pulling.out)
    }

    /// Asks the source for `ask.len` bytes at `ask.offset` into `ask.buf`.
    pub fn ask(&mut self, pad: SinkPad, ask: Ask) -> Result<(), RangeError> {
        if !self.pulls(pad) {
            return Err(RangeError::NotPulled);
        }
        if self.pulling.out >= self.credits {
            return Err(RangeError::Full);
        }
        let capacity = ask.buf.memory.capacity();
        if ask.len as usize > capacity {
            return Err(RangeError::Small { len: ask.len, capacity });
        }
        self.pulling.out += 1;
        self.pulling.asks.push_back(ask);
        Ok(())
    }

    pub fn end_asks(&mut self, pad: SinkPad) {
        if self.pulls(pad) {
            self.pulling.over = true;
        }
    }

    /// The next served range in `inputs` and its tag, in the order served.
    pub fn next_served(&mut self, pad: SinkPad, inputs: &mut Inputs<'_>) -> Option<(u64, Buffer)> {
        if !self.pulls(pad) {
            return None;
        }
        let buf = inputs.pop()?;
        self.pulling.out = self.pulling.out.saturating_sub(1);
        Some((self.pulling.tags.pop_front().unwrap_or(u64::MAX), buf))
    }

    /// Where the pull starts over, once after a pairing, a flush, a restart or a withdrawal.
    pub fn pull_reset(&mut self, pad: SinkPad) -> Option<SeekTarget> {
        if pad.0 != 0 {
            return None;
        }
        self.pulling.reset.take()
    }

    pub fn post(&mut self, msg: BusMessage) {
        self.bus.send(msg);
    }

    pub(crate) fn bus(&self) -> &BusSender {
        &self.bus
    }

    /// Running time on the pipeline clock, `NONE` without one.
    pub fn now(&self) -> Timestamp {
        match (&self.clock, &self.control) {
            (Some(c), Some((control, _))) => control.running(c.now()),
            (Some(c), None) => crate::time::running_time(c.now().0, self.base_raw()),
            (None, _) => Timestamp::NONE,
        }
    }

    /// The running time of stream time `pts`, through the timeline.
    pub fn running_time(&self, pts: Timestamp) -> Timestamp {
        match &self.control {
            Some((control, _)) if pts.is_some() => clamp(control.timeline.running(pts)),
            _ => pts,
        }
    }

    pub fn stream_time(&self, running: Timestamp) -> Timestamp {
        self.control.as_ref().map_or(running, |(control, _)| control.timeline.stream(running))
    }

    /// Claims the timeline in `start()`, one element per pipeline may retime.
    pub fn claim_timeline(&mut self) -> Result<(), Error> {
        let Some((control, _)) = &self.control else {
            return Ok(());
        };
        let element = self.element;
        let refused = |first| Error::Graph(GraphError::TimelineClaimed { element, first });
        control.claim(element).map_err(refused)
    }

    pub fn retime(&mut self, stream_start: Timestamp, rate: Rate) {
        if let Some((control, cursor)) = &self.control {
            control.retime(cursor, stream_start, rate);
        }
    }

    /// The running time `pts` is presented at, with the path's latency and the offset.
    fn running_deadline(&self, pts: Timestamp) -> Timestamp {
        let at = self.running_time(pts).saturating_add(self.path_latency);
        if at.is_none() { at } else { clamp(at.0 as i128 + self.offset as i128) }
    }

    /// How far past the deadline `pts` is on the pipeline clock, `None` without a clock, paused or
    /// not due.
    pub fn lateness(&self, pts: Timestamp) -> Option<Timestamp> {
        let c = self.clock.as_ref().filter(|_| !self.paused)?;
        let deadline =
            crate::time::deadline(self.base_raw(), self.running_deadline(pts), Timestamp::ZERO);
        let now = c.now();
        (deadline.is_some() && now >= deadline).then(|| now.saturating_sub(deadline))
    }

    /// Whether `pts` is due on the pipeline clock, arming a wake when it is not.
    pub fn due(&mut self, pts: Timestamp) -> bool {
        let Some(c) = &self.clock else { return true };
        if self.paused {
            return false;
        }
        let at = self.running_deadline(pts);
        let deadline = crate::time::deadline(self.base_raw(), at, Timestamp::ZERO);
        let now = c.now();
        if deadline.is_none() || now < deadline {
            self.wake_at = self.wake_at.min(at);
            return false;
        }
        if let Some((counters, gate)) = &self.trace
            && gate.load(Ordering::Relaxed)
        {
            counters.wait_lateness_ns.record(now.saturating_sub(deadline).nanos().unwrap_or(0));
        }
        true
    }

    /// Asks to be run again by running time `running`, re-armed each pass.
    pub fn wake_at(&mut self, running: Timestamp) {
        self.wake_at = self.wake_at.min(running);
    }

    /// Asks to be run again within `interval` of wall time.
    pub fn wake_within(&mut self, interval: Duration) {
        self.wake_within = self.wake_within.min(interval);
    }

    pub fn take_wake_within(&mut self) -> Duration {
        std::mem::replace(&mut self.wake_within, Duration::MAX)
    }

    pub fn take_wake_at(&mut self) -> Option<Timestamp> {
        let at = std::mem::replace(&mut self.wake_at, Timestamp::NONE);
        at.is_some().then_some(at)
    }

    /// The current value of a declared property, `None` while never set.
    pub fn prop(&self, name: &str) -> Option<Value> {
        let (descs, table) = self.props.as_ref()?;
        let idx = descs.iter().position(|p| p.name == name)?;
        table.get(idx)
    }

    pub fn set_props(&mut self, descs: &'static [crate::element::PropDesc], table: Arc<PropTable>) {
        self.props = Some((descs, table));
    }

    pub(crate) fn poll_prop_changes(&self) -> u64 {
        match &self.props {
            Some((_, table)) => table.take_dirty(),
            None => 0,
        }
    }

    pub(crate) fn prop_entry(&self, idx: usize) -> Option<(&'static str, Value)> {
        let (descs, table) = self.props.as_ref()?;
        let name = descs.get(idx)?.name;
        Some((name, table.get(idx)?))
    }

    pub fn feedback(&self, pad: SrcPad) -> Upstream {
        self.fb_in.get(pad.0 as usize).copied().unwrap_or_default()
    }

    /// Tells what feeds sink pad `pad`, latest wins, read at the top of its next pass.
    pub fn publish_feedback(&mut self, pad: SinkPad, feedback: Feedback) {
        let mut up = self.fb_out.get(pad.0 as usize).map_or(Upstream::default(), |o| o.0);
        up.set(feedback);
        self.put_feedback(pad, up);
    }

    fn put_feedback(&mut self, pad: SinkPad, up: Upstream) {
        let i = pad.0 as usize;
        if self.fb_out.len() <= i {
            self.fb_out.resize(i + 1, (Upstream::default(), false));
        }
        let (was, dirty) = &mut self.fb_out[i];
        *dirty |= *was != up;
        *was = up;
    }

    pub(crate) fn take_published(&mut self, pad: SinkPad) -> Option<Upstream> {
        let (up, dirty) = self.fb_out.get_mut(pad.0 as usize)?;
        std::mem::take(dirty).then_some(*up)
    }

    /// Src pad `pad` is told `up`, forwarded unchanged with one pad a side. What it held.
    pub(crate) fn told(&mut self, pad: SrcPad, up: Upstream) -> Upstream {
        let i = pad.0 as usize;
        if self.fb_in.len() <= i {
            self.fb_in.resize(i + 1, Upstream::default());
        }
        if (self.nsrcs, self.nsinks) == (1, 1) {
            self.put_feedback(SinkPad(0), up);
        }
        std::mem::replace(&mut self.fb_in[i], up)
    }

    pub fn scratch(&self) -> &Arena {
        &self.scratch
    }

    pub(crate) fn reset_scratch(&mut self) {
        self.scratch.reset();
    }

    fn out_slot(&mut self, pad: SrcPad) -> &mut Batch {
        let i = pad.0 as usize;
        if self.outs.len() <= i {
            self.outs.resize_with(i + 1, || Batch::new(self.out_format));
        }
        &mut self.outs[i]
    }

    /// Sizes the output table and counts the pads, so added pads get the ids past them.
    pub fn configure_pads(&mut self, nsrcs: usize, nsinks: usize) {
        self.outs = (0..nsrcs).map(|_| Batch::new(self.out_format)).collect();
        self.nsrcs = nsrcs as u32;
        self.nsinks = nsinks as u32;
    }

    pub(crate) fn grow_pads(&mut self, nsrcs: usize, nsinks: usize) {
        while self.outs.len() < nsrcs {
            self.outs.push(Batch::new(self.out_format));
        }
        if self.full_pads.len() < nsrcs {
            self.full_pads.resize(nsrcs, false);
        }
        (self.nsrcs, self.nsinks) = (nsrcs as u32, nsinks as u32);
    }

    pub(crate) fn add_offers(&mut self, offers: PadOffers) {
        self.negotiated.offers.retain(|o| o.key != offers.key);
        self.negotiated.offers.push(offers);
    }

    pub fn offer_src(&mut self, pad: SrcPad, offers: &[OfferSpec<'_>]) -> Result<(), Error> {
        self.offer(true, pad.0, offers)
    }

    pub fn offer_sink(&mut self, pad: SinkPad, offers: &[OfferSpec<'_>]) -> Result<(), Error> {
        self.offer(false, pad.0, offers)
    }

    fn offer(&mut self, src: bool, pad: u32, offers: &[OfferSpec<'_>]) -> Result<(), Error> {
        let element = self.element;
        let refused = |fault| Error::Offer { element, src, pad, fault };
        let Some((desc, names, found)) = self.probe.as_mut() else {
            return Err(refused(OfferFault::Frozen));
        };
        let statics = if src { desc.srcs } else { desc.sinks };
        let added = if src { &self.added_srcs } else { &self.added_sinks };
        let Some((name, _)) = pad_at(statics, added, pad).filter(|p| OfferDesc::is_probed(p.1))
        else {
            return Err(refused(OfferFault::NotProbed));
        };
        if found.iter().any(|f| f.key == (src, pad)) {
            return Err(refused(OfferFault::Twice));
        }
        admit(offers).map_err(refused)?;
        let lines = offers.iter().map(|o| offer_line(o).into()).collect();
        let names = [&mut names.formats, &mut names.fields, &mut names.values];
        let offers = Interned::intern(offers, names);
        found.push(PadOffers {
            key: (src, pad),
            name: name.into(),
            offers,
            lines,
            menu: crate::solve::NONE,
        });
        Ok(())
    }

    // Once per discovered stream, the owned name is a one-time cost.
    #[allow(clippy::disallowed_methods)]
    pub fn add_src_pad(&mut self, name: &str, offers: &'static [OfferDesc]) -> SrcPad {
        self.added_srcs.push(AddedPad { name: name.to_string(), offers });
        self.nsrcs += 1;
        SrcPad(self.nsrcs - 1)
    }

    pub fn src_pad_count(&self) -> u32 {
        self.nsrcs
    }

    // Once per pad, the owned name is a one-time cost.
    #[allow(clippy::disallowed_methods)]
    pub fn add_sink_pad(&mut self, name: &str, offers: &'static [OfferDesc]) -> SinkPad {
        self.added_sinks.push(AddedPad { name: name.to_string(), offers });
        self.nsinks += 1;
        SinkPad(self.nsinks - 1)
    }

    pub(crate) fn take_added_pads(&mut self) -> (Vec<AddedPad>, Vec<AddedPad>) {
        (std::mem::take(&mut self.added_srcs), std::mem::take(&mut self.added_sinks))
    }

    pub fn output_on(&mut self, pad: SrcPad) -> &mut Batch {
        self.out_slot(pad)
    }

    /// Takes the output batch of a src pad, replacing it with a recycled shell.
    pub fn take_output(&mut self, pad: SrcPad) -> Batch {
        let fmt = self.out_format;
        let replacement = match self.spare_outputs.pop() {
            Some(mut shell) => {
                shell.format = fmt;
                shell
            }
            None => Batch::new(fmt),
        };
        std::mem::replace(self.out_slot(pad), replacement)
    }

    pub(crate) fn share_output(&mut self, pad: SrcPad) -> Batch {
        let fmt = self.out_format;
        let mut copy = self.spare_outputs.pop().unwrap_or_else(|| Batch::new(fmt));
        copy.format = fmt;
        self.out_slot(pad).share_into(&mut copy);
        copy
    }

    pub(crate) fn recycle_shell(&mut self, mut shell: Batch) {
        if self.spare_outputs.len() < self.spare_cap {
            shell.clear();
            self.spare_outputs.push(shell);
        }
    }

    pub(crate) fn bump_spare_cap(&mut self, n: usize) {
        self.spare_cap += n;
    }

    pub(crate) fn total_output(&self) -> (u64, u64) {
        self.outs.iter().fold((0u64, 0u64), |(n, b), o| (n + o.len() as u64, b + o.total_bytes()))
    }

    /// The newest stamp put out in this pass on any pad, `NONE` for none.
    pub fn newest_out(&self) -> Timestamp {
        self.outs.iter().map(Batch::newest).filter(|t| t.is_some()).max().unwrap_or(Timestamp::NONE)
    }

    pub(crate) fn newest_output_pts(&self) -> impl Iterator<Item = (usize, Timestamp)> + '_ {
        self.outs.iter().map(Batch::newest).enumerate()
    }

    pub(crate) fn clear_outputs(&mut self) {
        for out in &mut self.outs {
            out.clear();
        }
        self.serving.tags.clear();
    }

    pub(crate) fn take_input(&mut self) -> Batch {
        std::mem::replace(&mut self.input, Batch::new(self.out_format))
    }

    pub(crate) fn set_input(&mut self, input: Batch) {
        self.input = input;
    }

    pub fn input_append(&mut self, other: &mut Batch) {
        self.input.append(other);
    }

    pub(crate) fn due_input_event(&mut self) -> Option<Event> {
        self.input.due_event()
    }

    pub(crate) fn any_input_event_on(&mut self, pad: SinkPad) -> Option<Event> {
        self.pad_inputs.get_mut(pad.0 as usize)?.next_event_any()
    }

    pub fn input_len(&self) -> usize {
        self.input.len()
    }

    pub(crate) fn input_left(&self, pad: Option<SinkPad>) -> (u64, Timestamp) {
        let input = match pad {
            None => Some(&self.input),
            Some(pad) => self.pad_inputs.get(pad.0 as usize),
        };
        let front = |b: &Batch| b.rows().first().map_or(Timestamp::NONE, |row| row.pts);
        input.map_or((0, Timestamp::NONE), |b| (b.total_bytes(), front(b)))
    }

    pub(crate) fn pending(&self) -> usize {
        let inputs = self.input.len() + self.pad_inputs.iter().map(Batch::len).sum::<usize>();
        inputs + self.io.inbox.len() + self.serving.asks.len()
    }

    pub(crate) fn held(&self) -> usize {
        let done = self.io.inbox.iter().filter(|c| c.buf.is_some()).count();
        let asks = self.serving.asks.len() + self.pulling.asks.len();
        self.input.len() + self.pad_inputs.iter().map(Batch::len).sum::<usize>() + done + asks
    }

    pub(crate) fn inputs_empty(&self) -> bool {
        self.input.is_empty()
            && !self.input.has_events()
            && self.pad_inputs.iter().all(|b| b.is_empty() && !b.has_events())
    }

    pub(crate) fn append_input_on(&mut self, pad: SinkPad, other: &mut Batch) {
        let i = pad.0 as usize;
        if self.pad_inputs.len() <= i {
            self.pad_inputs.resize_with(i + 1, || Batch::new(self.out_format));
        }
        self.pad_inputs[i].append(other);
    }

    /// Takes a sink pad's accumulated input, for an aggregating element.
    pub fn take_input_on(&mut self, pad: SinkPad) -> Batch {
        let i = pad.0 as usize;
        if self.pad_inputs.len() <= i {
            self.pad_inputs.resize_with(i + 1, || Batch::new(self.out_format));
        }
        let fmt = self.out_format;
        let replacement = match self.spare_inputs.pop() {
            Some(mut shell) => {
                shell.format = fmt;
                shell
            }
            None => Batch::new(fmt),
        };
        std::mem::replace(&mut self.pad_inputs[i], replacement)
    }

    pub fn front_input_on(&self, pad: SinkPad) -> Option<&Buffer> {
        self.pad_inputs.get(pad.0 as usize)?.front()
    }

    pub fn pop_input_on(&mut self, pad: SinkPad) -> Option<Buffer> {
        self.pad_inputs.get_mut(pad.0 as usize)?.pop_front()
    }

    pub(crate) fn input_len_on(&self, pad: SinkPad) -> usize {
        self.pad_inputs.get(pad.0 as usize).map_or(0, Batch::len)
    }

    pub fn push_event(&mut self, pad: SrcPad, event: Event) {
        self.out_slot(pad).push_event(event);
    }

    /// Src pad `pad` ends until a seek or relink, its links end while the other pads run on.
    pub fn end_src(&mut self, pad: SrcPad) {
        self.out_slot(pad).eos = true;
    }

    /// Returns a spent batch from [`take_input_on`](Self::take_input_on) for reuse.
    pub fn recycle_input(&mut self, mut batch: Batch) {
        if self.spare_inputs.len() < 2 * self.pad_inputs.len().max(1) {
            batch.clear();
            self.spare_inputs.push(batch);
        }
    }

    /// Sink pad `pad` refused the format its upstream changed to, or took one again.
    pub(crate) fn set_refused(&mut self, pad: SinkPad, on: bool) -> bool {
        let was = self.refused.get(pad.0 as usize).copied().unwrap_or(false);
        set_flag(&mut self.refused, pad.0 as usize, on);
        was
    }

    /// Drop what waits on `pad` up to its next event while it is behind a refused format.
    pub(crate) fn drop_refused(&mut self, pad: SinkPad, fan_in: bool) -> u64 {
        if !self.refused.get(pad.0 as usize).copied().unwrap_or(false) {
            return 0;
        }
        let input = match fan_in {
            true => self.pad_inputs.get_mut(pad.0 as usize),
            false => Some(&mut self.input),
        };
        let Some(input) = input else { return 0 };
        let mut n = 0;
        while input.pop_front().is_some() {
            n += 1;
        }
        n
    }

    pub fn is_pad_closed(&self, pad: SinkPad) -> bool {
        self.pad_eos.get(pad.0 as usize).copied().unwrap_or(false)
    }

    pub(crate) fn set_pad_closed(&mut self, pad: SinkPad) {
        let i = pad.0 as usize;
        if self.pad_eos.len() <= i {
            self.pad_eos.resize(i + 1, false);
        }
        self.pad_eos[i] = true;
    }

    pub(crate) fn reopen_pad(&mut self, pad: SinkPad) {
        if let Some(closed) = self.pad_eos.get_mut(pad.0 as usize) {
            *closed = false;
        }
    }

    pub(crate) fn discard_input(&mut self, pad: Option<SinkPad>) {
        match pad.map(|p| p.0 as usize) {
            None => {
                self.input.clear();
                self.pulling.tags.clear();
            }
            Some(i) => self.pad_inputs.get_mut(i).into_iter().for_each(Batch::clear),
        }
    }

    pub(crate) fn move_input(&mut self, pad: SinkPad, to_pad: bool) {
        if to_pad {
            let mut input = self.take_input();
            self.append_input_on(pad, &mut input);
        } else if let Some(waiting) = self.pad_inputs.get_mut(pad.0 as usize) {
            self.input.append(waiting);
        }
    }

    pub(crate) fn clear_pads_closed(&mut self) {
        self.pad_eos.iter_mut().for_each(|p| *p = false);
    }

    pub(crate) fn submissions(&mut self) -> (&mut Vec<Submission>, &[u8]) {
        (&mut self.io.outbox, &self.io.names)
    }

    pub(crate) fn names_spent(&mut self) {
        self.io.names.clear();
    }

    pub(crate) fn deliver_completion(&mut self, c: Completion) {
        self.io.flying = self.io.flying.saturating_sub(1);
        self.io.inbox.push_back(c);
    }

    pub(crate) fn inbox_empty(&self) -> bool {
        self.io.inbox.is_empty()
    }

    pub(crate) fn io_credits(&self) -> u32 {
        self.credits
    }

    pub(crate) fn io_flying(&self) -> u32 {
        self.io.flying
    }

    /// Drops staged pipeline data on a flush, keeping state events and the IO mailbox.
    // Clearing the mailbox would desync a source's count of reads in flight.
    pub fn discard_buffers(&mut self) {
        self.input.retain_state();
        for o in &mut self.outs {
            o.retain_state();
        }
        for i in &mut self.pad_inputs {
            i.retain_state();
        }
        for a in &mut self.announced {
            a.at = 0;
        }
        self.serving.tags.clear();
        self.pulling.tags.clear();
        self.fb_in.fill(Upstream::default());
        self.fb_out.fill((Upstream::default(), false));
    }
}

fn set_slot(table: &mut Vec<Option<FixedFormat>>, index: u32, format: FixedFormat) {
    let i = index as usize;
    if table.len() <= i {
        table.resize(i + 1, None);
    }
    table[i] = Some(format);
}

impl Loggable for Ctx {
    #[inline]
    fn enabled(&self, level: Level) -> bool {
        match &self.log {
            Some(log) => log.enabled(level),
            None => false,
        }
    }

    fn emit(&self, level: Level, event: &'static str, fields: &[Field], text: Text) {
        if let Some(log) = &self.log {
            log.emit(level, event, fields, text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Bus;
    use crate::log::{FieldValue, LevelFilter, Log, log_channel};
    use std::sync::Arc;

    fn test_ctx(element: ElementId) -> Ctx {
        let pool = Pool::bounded(1024, 4);
        let (bus, _rx) = Bus::channel();
        Ctx::new(pool, bus, element, FormatId(0), 4)
    }

    #[test]
    fn a_src_pad_reads_the_family_its_consumers_name_first() {
        use crate::format::OfferDesc;
        const BOTH: [&str; 2] = ["h265/annexb", "h265/hvcc"];
        let stored: &[OfferDesc] = &[OfferDesc::any("h265/hvcc"), OfferDesc::any("h265/annexb")];
        let plain: &[OfferDesc] = &[OfferDesc::any("vp9"), OfferDesc::any("h265/annexb")];
        let other: &[OfferDesc] = &[OfferDesc::any("vp9"), OfferDesc::any("bytes")];
        let any: &[OfferDesc] = &[OfferDesc::wildcard(), OfferDesc::any("h265/hvcc")];
        let (mut ctx, pad) = (test_ctx(ElementId(0)), SrcPad(1));
        assert_eq!(ctx.src_peers_prefer(pad, &BOTH), None, "a pad that feeds nothing");
        type Case<'a> = (&'a [&'a [OfferDesc]], Option<&'a str>, &'a str);
        let cases: [Case; 8] = [
            (&[stored], Some("h265/hvcc"), "the first its one consumer names"),
            (&[plain], Some("h265/annexb"), "the only one its consumer names"),
            (&[stored, stored], Some("h265/hvcc"), "two that agree"),
            (&[stored, plain], None, "two that disagree"),
            (&[other], None, "a consumer that names neither"),
            (&[stored, other], None, "one of two names neither"),
            (&[any], None, "a consumer that takes any family"),
            (&[], None, "every link gone"),
        ];
        for (menus, want, what) in cases {
            ctx.set_src_peers(pad, menus);
            assert_eq!(ctx.src_peers_prefer(pad, &BOTH), want, "{what}");
        }
        ctx.set_src_peers(pad, &[stored]);
        assert_eq!(ctx.src_peers_prefer(SrcPad(0), &BOTH), None, "another pad");
        assert_eq!(ctx.src_peers_prefer(pad, &["h265/annexb"]), Some("h265/annexb"));
        assert_eq!(ctx.src_peers_prefer(pad, &[]), None, "nothing asked");
    }

    #[test]
    fn a_pad_is_held_to_what_every_link_of_it_takes() {
        let (rate, pix) = (FieldId(1), FieldId(2));
        let ids = |v: &[u32]| v.iter().map(|i| Value::Id(ValueId(*i))).collect::<Vec<_>>();
        let mut ctx = test_ctx(ElementId(1));
        assert_eq!(ctx.src_takes(SrcPad(0), pix), None, "unlinked");
        let mut f = PadFormats::default();
        f.hold_to(SrcPad(1), &[(pix, ids(&[3, 4, 5]))]);
        f.hold_to(SrcPad(1), &[(pix, ids(&[5, 3])), (rate, vec![Value::Int(48_000)])]);
        ctx.set_negotiated(f);
        assert_eq!(
            ctx.src_takes(SrcPad(1), pix),
            Some(&ids(&[3, 5])[..]),
            "both links, in the first one's order"
        );
        assert_eq!(
            ctx.src_takes(SrcPad(1), rate),
            Some(&[Value::Int(48_000)][..]),
            "a field one link limits"
        );
        assert_eq!(ctx.src_takes(SrcPad(0), pix), None, "a pad with no link of its own");
        ctx.set_src_takes(SrcPad(0), rate, &[Value::Int(44_100)]);
        assert_eq!(ctx.src_takes(SrcPad(0), rate), Some(&[Value::Int(44_100)][..]));
    }

    #[test]
    fn ctx_with_no_log_is_disabled_and_silent() {
        let ctx = test_ctx(ElementId(1));
        assert!(!ctx.enabled(Level::Error));
        assert!(!ctx.enabled(Level::Trace));

        let mut calls = 0u32;
        let mut arg = || {
            calls += 1;
            7u64
        };
        crate::log!(&ctx, Level::Error, "suppressed", v = arg());
        assert_eq!(calls, 0, "no Log, arguments must not be evaluated");
    }

    #[test]
    fn log_through_ctx_reaches_the_drain_stamped_with_element() {
        let filter = Arc::new(LevelFilter::with_level(Level::Info));
        let (sink, drain) = log_channel(64);
        let mut log = Log::new(sink, filter);
        log.set_element(ElementId(5), "testsrc");

        let mut ctx = test_ctx(ElementId(5));
        ctx.set_log(log);

        assert!(ctx.enabled(Level::Info));
        crate::log!(&ctx, Level::Info, "hello", n = 42u64, ok = true);

        let rec = drain.try_next().expect("record reached the drain");
        assert_eq!(rec.event, "hello");
        assert_eq!(rec.element, ElementId(5), "stamped with the element id");
        assert_eq!(rec.name, "testsrc", "stamped with the element name");
        assert_eq!(rec.level, Level::Info);
        assert_eq!(rec.fields().len(), 2);
        assert_eq!(rec.fields()[0].key, "n");
        assert!(matches!(rec.fields()[0].val, FieldValue::Uint(42)));
        assert!(matches!(rec.fields()[1].val, FieldValue::Bool(true)));
        assert!(drain.try_next().is_none(), "exactly one record");
    }

    #[test]
    fn now_and_due_track_the_installed_clock() {
        use crate::clock::MockClock;
        use std::sync::atomic::{AtomicBool, AtomicI64};
        let ms = Timestamp::from_millis;

        let mut ctx = test_ctx(ElementId(1));
        assert!(ctx.now().is_none());
        assert!(ctx.due(Timestamp::from_secs(1)) && ctx.due(Timestamp::NONE));
        assert!(ctx.take_wake_at().is_none(), "a due answer arms nothing");

        let clock = MockClock::new();
        let base = Arc::new(AtomicI64::new(ms(100).0 as i64));
        ctx.set_clock(Arc::new(clock.clone()), Arc::clone(&base));
        assert_eq!(ctx.now(), Timestamp::ZERO, "at the base instant, running time is 0");
        clock.advance(ms(150));
        assert_eq!(ctx.now(), ms(50), "running = clock(150) - base(100)");

        assert!(ctx.due(ms(50)), "exactly at the deadline is due");
        assert!(!ctx.due(ms(51)));
        assert_eq!(ctx.take_wake_at(), Some(ms(51)), "early arms the wake for the deadline");
        assert!(!ctx.due(Timestamp::NONE));
        assert!(ctx.take_wake_at().is_none(), "NONE is never due and arms nothing");

        let counters: Arc<crate::counters::ElementCounters> = Arc::default();
        ctx.set_trace(Arc::clone(&counters), Arc::new(AtomicBool::new(true)));
        ctx.set_path_latency(ms(20));
        assert!(!ctx.due(ms(40)), "40 + 20 is past running time 50");
        assert_eq!(ctx.take_wake_at(), Some(ms(60)));
        assert_eq!(counters.wait_lateness_ns.snapshot().count, 0, "not due, not traced");
        assert!(ctx.due(ms(27)));
        let late = counters.wait_lateness_ns.snapshot();
        assert_eq!((late.count, late.max_ns), (1, ms(3).0), "3 ms past 27 + 20");

        base.store(ms(90).0 as i64, Ordering::Release);
        assert!(ctx.due(ms(40)), "running time is 60 now");

        assert_eq!(ctx.lateness(ms(40)), Some(Timestamp::ZERO), "at its deadline, late by nothing");
        assert_eq!(ctx.lateness(ms(10)), Some(ms(30)), "10 + 20 against running time 60");
        assert_eq!(ctx.lateness(ms(41)), None, "not due is not late");
        assert_eq!(ctx.lateness(Timestamp::NONE), None);

        ctx.set_paused(true);
        assert!(!ctx.due(Timestamp::ZERO));
        assert!(ctx.take_wake_at().is_none(), "no deadline is armed while paused");
        assert_eq!(ctx.lateness(ms(10)), None, "nothing is late while paused");
        ctx.set_paused(false);
        assert!(ctx.due(Timestamp::ZERO));
    }

    #[test]
    fn a_buffer_is_before_the_presentation_point_only_when_none_of_it_is_at_or_after() {
        let ms = Timestamp::from_millis;
        let (zero, none) = (Timestamp::ZERO, Timestamp::NONE);
        let mut ctx = test_ctx(ElementId(1));
        assert!(ctx.presentation_point().is_none());
        assert!(!ctx.before_presentation(zero, zero) && !ctx.before_presentation(zero, ms(5)));

        ctx.set_presentation(ms(100));
        assert_eq!(ctx.presentation_point(), Some(ms(100)));
        assert!(
            ctx.before_presentation(ms(60), zero)
                && ctx.before_presentation(Timestamp(ms(100).0 - 1), zero)
        );
        assert!(!ctx.before_presentation(ms(100), zero) && !ctx.before_presentation(ms(140), zero));
        assert!(ctx.before_presentation(ms(60), ms(20)) && ctx.before_presentation(ms(60), ms(40)));
        assert!(!ctx.before_presentation(ms(60), ms(41)), "one millisecond of it is to be heard");
        assert!(!ctx.before_presentation(ms(100), ms(20)));
        assert!(!ctx.before_presentation(none, zero) && !ctx.before_presentation(none, ms(20)));
        assert!(!ctx.before_presentation(ms(60), none));

        let target = SeekTarget { to_byte: 7, at: ms(120) };
        ctx.set_seek_target(1, ms(300), Some(target));
        assert_eq!(
            ctx.seek_target(),
            Some(target),
            "where it reads is not where presentation starts"
        );
        assert_eq!(ctx.presentation_point(), Some(ms(300)));
        assert!(ctx.before_presentation(ms(140), zero));
        ctx.set_seek_target(2, none, None);
        assert_eq!(ctx.seek_target(), None, "and a seek that names no target for it leaves none");
        assert!(ctx.presentation_point().is_none() && !ctx.before_presentation(zero, zero));
    }

    #[test]
    fn a_preview_is_taken_once_per_seek_that_armed_it() {
        let ms = Timestamp::from_millis;
        let mut ctx = test_ctx(ElementId(1));
        assert!(!ctx.take_preview(), "none without a seek");
        ctx.set_seek_target(1, ms(300), None);
        assert!(!ctx.take_preview(), "an accurate seek has none");
        ctx.set_seek_target(2, ms(300), None);
        ctx.arm_preview();
        assert!(ctx.take_preview() && !ctx.take_preview(), "one picture");
        ctx.set_seek_target(3, ms(400), None);
        ctx.arm_preview();
        ctx.set_presentation(ms(500));
        assert!(!ctx.take_preview(), "a skip after the seek drops it");
        ctx.set_seek_target(4, Timestamp::NONE, None);
        ctx.arm_preview();
        assert!(!ctx.take_preview(), "nothing is before no point");
    }

    #[test]
    fn a_counted_drop_lands_in_the_elements_drops_and_nowhere_without_counters() {
        use std::sync::atomic::AtomicBool;
        let ctx = test_ctx(ElementId(1));
        ctx.count_drop();
        assert!(!ctx.command_pending(), "and outside a run no command is pending");
        let mut ctx = test_ctx(ElementId(1));
        let counters: Arc<crate::counters::ElementCounters> = Arc::default();
        ctx.set_trace(Arc::clone(&counters), Arc::new(AtomicBool::new(false)));
        ctx.count_drop();
        ctx.count_drop();
        let c = counters.snapshot();
        assert_eq!((c.drops, c.buffers_in), (2, 0), "with tracing off, it is no trace");
        assert_eq!(counters.wait_lateness_ns.snapshot().count, 0, "and nothing late");
    }

    #[test]
    fn the_wake_requests_keep_the_earliest_and_are_spent_by_the_take() {
        let mut ctx = test_ctx(ElementId(1));

        assert_eq!(ctx.take_wake_within(), Duration::MAX);
        ctx.wake_within(Duration::from_millis(8));
        ctx.wake_within(Duration::from_millis(3));
        assert_eq!(ctx.take_wake_within(), Duration::from_millis(3));
        assert_eq!(ctx.take_wake_within(), Duration::MAX);
        ctx.wake_at(Timestamp::from_millis(9));
        ctx.wake_at(Timestamp::from_millis(4));
        assert_eq!(ctx.take_wake_at(), Some(Timestamp::from_millis(4)));
        assert_eq!(ctx.take_wake_at(), None);
    }

    #[test]
    fn ctx_gate_suppresses_levels_below_threshold() {
        let filter = Arc::new(LevelFilter::with_level(Level::Warn));
        let (sink, drain) = log_channel(64);
        let mut log = Log::new(sink, Arc::clone(&filter));
        log.set_element(ElementId(3), "testsrc");

        let mut ctx = test_ctx(ElementId(3));
        ctx.set_log(log);

        crate::log!(&ctx, Level::Debug, "below", x = 1);
        crate::log!(&ctx, Level::Error, "boom", code = 500u32);
        let rec = drain.try_next().expect("error passed the gate");
        assert_eq!(rec.event, "boom");
        assert!(drain.try_next().is_none(), "debug was gated out");

        filter.set(Some(Level::Trace));
        crate::log!(&ctx, Level::Debug, "now_visible", x = 2);
        let rec = drain.try_next().expect("debug passes once the gate is raised");
        assert_eq!(rec.event, "now_visible");
    }

    #[test]
    fn each_src_pad_is_its_own_output_and_nothing_routes_by_guess() {
        let mut ctx = test_ctx(ElementId(1));
        ctx.configure_pads(1, 1);
        let row = ctx.alloc();
        ctx.out(SrcPad(0)).push(row);
        let stray = ctx.alloc();
        ctx.out(SrcPad(1)).push(stray);
        ctx.push_event(SrcPad(1), Event::Eos);
        assert_eq!((ctx.output_on(SrcPad(0)).len(), ctx.output_on(SrcPad(1)).len()), (1, 1));
        assert!(!ctx.output_on(SrcPad(0)).has_events() && ctx.output_on(SrcPad(1)).has_events());
        assert_eq!(ctx.output_on(SrcPad(0)).len(), 1, "the hand-off moves the first src pad");
        ctx.announce_format(SrcPad(1), "audio/raw", &[]);
        ctx.forward_format(SrcPad(0), FixedFormat::new(FormatId(0)));
        let pads: Vec<(SrcPad, u32)> =
            ctx.take_announcements().iter().map(|a| (a.pad, a.at)).collect();
        assert_eq!(pads, [(SrcPad(1), 1), (SrcPad(0), 1)], "each keeps the pad it named");
    }

    #[test]
    fn added_pads_are_numbered_per_side_past_the_pads_the_element_has() {
        static NONE: [OfferDesc; 0] = [];
        let mut ctx = test_ctx(ElementId(1));
        ctx.configure_pads(1, 2);
        assert_eq!(ctx.add_src_pad("video_0", &NONE), SrcPad(1));
        assert_eq!(ctx.add_sink_pad("feed_0", &NONE), SinkPad(2));
        assert_eq!(ctx.add_src_pad("audio_0", &NONE), SrcPad(2));
        let (srcs, sinks) = ctx.take_added_pads();
        let names = |pads: &[AddedPad]| pads.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&srcs), ["video_0", "audio_0"]);
        assert_eq!(names(&sinks), ["feed_0"]);
        assert!(ctx.take_added_pads().0.is_empty(), "drained");
        let row = ctx.alloc();
        ctx.out(SrcPad(2)).push(row);
        assert_eq!(ctx.output_on(SrcPad(2)).len(), 1);
    }

    #[test]
    fn the_two_sides_keep_separate_tables() {
        let mut ctx = test_ctx(ElementId(1));
        let fmt = |family| FixedFormat::new(FormatId(family));
        let family = |f: Option<&FixedFormat>| f.map(|f| f.family.0);
        let (sinks, srcs) = (vec![Some(fmt(1))], vec![None, Some(fmt(2))]);
        ctx.set_negotiated(PadFormats { sinks, srcs, ..Default::default() });
        assert_eq!(family(ctx.negotiated_sink(SinkPad(0))), Some(1));
        assert_eq!(family(ctx.negotiated_src(SrcPad(0))), None, "linked on the sink side only");
        assert_eq!(family(ctx.negotiated_src(SrcPad(1))), Some(2));
        assert_eq!(family(ctx.negotiated_sink(SinkPad(1))), None, "past the table is unlinked");
        ctx.set_negotiated_sink(SinkPad(1), fmt(3));
        ctx.set_negotiated_src(SrcPad(0), fmt(4));
        assert_eq!(family(ctx.negotiated_sink(SinkPad(1))), Some(3));
        assert_eq!(family(ctx.negotiated_src(SrcPad(1))), Some(2));
        assert_eq!(family(ctx.negotiated_src(SrcPad(0))), Some(4));
        assert_eq!(family(ctx.negotiated_sink(SinkPad(0))), Some(1));

        ctx.set_pad_closed(SinkPad(1));
        assert!(ctx.is_pad_closed(SinkPad(1)) && !ctx.is_pad_closed(SinkPad(0)));
        ctx.set_linked_pads(vec![true, false]);
        assert!(ctx.pad_linked(SrcPad(0)) && !ctx.pad_linked(SrcPad(1)));
        assert!(ctx.pad_linked(SrcPad(9)), "an unknown pad fails open");
    }

    fn completion(ctx: &mut Ctx, user: u64) -> Completion {
        Completion {
            op: crate::io::OpId(user),
            user,
            result: crate::io::IoResult::Ok(0),
            buf: Some(ctx.alloc()),
        }
    }

    #[test]
    fn io_mailbox_drains_fifo_and_keeps_its_storage() {
        let (bus, _rx) = Bus::channel();
        let mut ctx = Ctx::new(Pool::bounded(64, 8), bus, ElementId(1), FormatId(0), 4);
        assert!(ctx.inbox_empty());

        let mut next = 0u64;
        let mut want = 0u64;
        for round in 0..6 {
            for _ in 0..3 {
                let c = completion(&mut ctx, next);
                ctx.deliver_completion(c);
                next += 1;
            }
            let cap = ctx.io.inbox.capacity();
            ctx.discard_buffers();
            assert!(!ctx.inbox_empty(), "round {round}: discard cleared the mailbox");
            let take = if round % 2 == 0 { 2 } else { 4 };
            for _ in 0..take {
                let got = ctx.io().next_completion().expect("a completion is pending");
                assert_eq!(got.user, want, "completions drain in delivery order");
                want += 1;
            }
            assert_eq!(ctx.io.inbox.capacity(), cap, "draining keeps the storage");
        }
        assert_eq!(want, next, "every delivered completion was drained");
        assert!(ctx.inbox_empty());
        assert!(ctx.io().next_completion().is_none());
    }
}
