//! Stacked state machines for sync, PSI section assembly and PES reassembly. Tables are sized
//! from [`Limits`] in [`Demux::new`], so a warmed up demuxer does not allocate.

use std::fmt;

use crate::packet::{
    PACKET_LEN, PF_DISCONTINUITY, PF_PAYLOAD, PF_PCR, PF_PCR_INVALID, PF_RANDOM_ACCESS,
    PF_UNIT_START, PID_COUNT, PID_NULL, PID_PAT, Packet, PacketError, SYNC_BYTE, parse_packet,
    payload, pid_of,
};
use crate::pes::{
    PES_HEADER_MAX, PH_BAD_TIMESTAMP, PH_DATA_ALIGNED, PH_PADDING, PH_SCRAMBLED, PesError,
    PesHeader, parse_pes_header,
};
use crate::psi::{
    Encryption, PSI_SECTION_MAX, PsiError, SECTION_MAX, SF_DESCRIPTORS_TRUNCATED, SectionHeader,
    StreamInfo, TABLE_ID_PAT, TABLE_ID_PMT, carries_pes, check_section, next_es, pat_entry,
    pat_rows, pid_is_assignable, pmt_head, registration_of, section_total_len, stream_info,
};
use crate::sync::{
    ANY_SIZE, EXTRA_HEADER_LEN, FRAMED_MAX, PacketSize, arrival_time_stamp, framed_len, only, scan,
    sync_offset,
};

/// Room for a whole PES header plus one packet payload, a split header is collected in the buffer.
pub const MIN_PES_BYTES: u32 = 512;
const _: () = assert!(MIN_PES_BYTES as usize >= PES_HEADER_MAX + PACKET_LEN);

/// TR 101 290 5.2.1 proposes five, cited from memory.
pub const SYNC_CONFIRM_DEFAULT: u8 = 5;
/// Bounds the search window to 16 * 204 + 4 = 3268 bytes. No spec basis.
pub const SYNC_CONFIRM_MAX: u8 = 16;
/// Sync positions after a loss that try only the lost size, and the most bytes one
/// [`AnomalyKind::SyncSkipped`] reports. No spec basis.
pub const RESYNC_BUDGET: u32 = 16 * 1024;
const _: () =
    assert!(RESYNC_BUDGET as usize > SYNC_CONFIRM_MAX as usize * FRAMED_MAX + EXTRA_HEADER_LEN);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// 1 to 1024, the rest raise [`AnomalyKind::ProgramLimit`] and are ignored.
    pub max_programs: u16,
    /// 1 to 256, the rest raise [`AnomalyKind::StreamLimit`] and are ignored.
    pub max_streams_per_program: u16,
    /// 1 to 8192, the rest raise [`AnomalyKind::PesPidLimit`] and are listed but not reassembled.
    pub max_pes_pids: u16,
    /// [`MIN_PES_BYTES`] to 2^30, a longer unit raises [`AnomalyKind::PesTooLarge`] and is
    /// dropped.
    pub max_pes_bytes: u32,
    /// Reserved when a PES PID is first mapped, at most `max_pes_bytes`.
    pub pes_reserve_bytes: u32,
    /// 64 to 4096, a longer section raises [`AnomalyKind::SectionTooLong`] and is skipped.
    pub max_section_bytes: u16,
    /// 1 to [`SYNC_CONFIRM_MAX`]. At 1 every sync byte passes for 188, so 192 and 204 never lock.
    pub sync_confirm: u8,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_programs: 64,
            max_streams_per_program: 32,
            max_pes_pids: 64,
            max_pes_bytes: 4 << 20,
            pes_reserve_bytes: 64 << 10,
            max_section_bytes: PSI_SECTION_MAX as u16,
            sync_confirm: SYNC_CONFIRM_DEFAULT,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub limits: Limits,
    /// Reassemble every clear PES stream unless deselected, else only [`Demux::select`]ed PIDs.
    pub select_all: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { limits: Limits::default(), select_all: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    Programs,
    StreamsPerProgram,
    PesPids,
    PesBytes,
    PesReserve,
    SectionBytes,
    SyncConfirm,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ConfigError::Programs => "max_programs outside 1 to 1024",
            ConfigError::StreamsPerProgram => "max_streams_per_program outside 1 to 256",
            ConfigError::PesPids => "max_pes_pids outside 1 to 8192",
            ConfigError::PesBytes => "max_pes_bytes outside 512 to 2^30",
            ConfigError::PesReserve => "pes_reserve_bytes above max_pes_bytes",
            ConfigError::SectionBytes => "max_section_bytes outside 64 to 4096",
            ConfigError::SyncConfirm => "sync_confirm outside 1 to 16",
        })
    }
}

impl std::error::Error for ConfigError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AnomalyKind {
    SyncLost,
    /// Detail is the byte count, at most [`RESYNC_BUDGET`] while the search goes on.
    SyncSkipped,
    /// Detail is the new size in bytes.
    PacketSizeChanged,
    TransportError,
    /// Detail is adaptation_field_length, zero for the reserved control value.
    AdaptationField,
    PcrInvalid,
    /// Payload ignored while it lasts. Detail is transport_scrambling_control.
    Scrambled,
    /// Detail is the expected value times 256 plus the value found.
    ContinuityLoss,
    Duplicate,
    PsiPointer,
    /// Detail is the size.
    SectionTooLong,
    /// Detail is the table_id.
    SectionCrc,
    /// Passed the CRC but is malformed, or names a PID it may not use.
    PsiMalformed,
    /// Detail is the section_number of a PAT section other than zero.
    PatSectionIgnored,
    /// Detail is the number ignored.
    ProgramLimit,
    /// Detail is the number ignored.
    StreamLimit,
    PesPidLimit,
    /// An elementary PID that is also a map PID, treated as PSI only.
    PidConflict,
    /// Listed, not reassembled unless selected by PID. Detail is the stream_type.
    EncryptedStream,
    DescriptorTruncated,
    PesStartCode,
    /// Malformed or cut off by the next unit.
    PesHeader,
    /// The unit carries no timestamp.
    PesTimestamp,
    /// The unit was dropped. Detail is the declared size, or the limit when unbounded.
    PesTooLarge,
    /// Detail is the number of bytes missing.
    PesTruncated,
    /// Buffer growth failed, the unit was dropped.
    PesAlloc,
}

impl AnomalyKind {
    /// A short tag for a log field.
    pub const fn name(self) -> &'static str {
        match self {
            AnomalyKind::SyncLost => "sync_lost",
            AnomalyKind::SyncSkipped => "sync_skipped",
            AnomalyKind::PacketSizeChanged => "packet_size_changed",
            AnomalyKind::TransportError => "transport_error",
            AnomalyKind::AdaptationField => "adaptation_field",
            AnomalyKind::PcrInvalid => "pcr_invalid",
            AnomalyKind::Scrambled => "scrambled",
            AnomalyKind::ContinuityLoss => "continuity_loss",
            AnomalyKind::Duplicate => "duplicate",
            AnomalyKind::PsiPointer => "psi_pointer",
            AnomalyKind::SectionTooLong => "section_too_long",
            AnomalyKind::SectionCrc => "section_crc",
            AnomalyKind::PsiMalformed => "psi_malformed",
            AnomalyKind::PatSectionIgnored => "pat_section_ignored",
            AnomalyKind::ProgramLimit => "program_limit",
            AnomalyKind::StreamLimit => "stream_limit",
            AnomalyKind::PesPidLimit => "pes_pid_limit",
            AnomalyKind::PidConflict => "pid_conflict",
            AnomalyKind::EncryptedStream => "encrypted_stream",
            AnomalyKind::DescriptorTruncated => "descriptor_truncated",
            AnomalyKind::PesStartCode => "pes_start_code",
            AnomalyKind::PesHeader => "pes_header",
            AnomalyKind::PesTimestamp => "pes_timestamp",
            AnomalyKind::PesTooLarge => "pes_too_large",
            AnomalyKind::PesTruncated => "pes_truncated",
            AnomalyKind::PesAlloc => "pes_alloc",
        }
    }
}

pub const ANOMALY_KINDS: usize = AnomalyKind::PesAlloc as usize + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anomaly {
    pub offset: u64,
    /// Meaning depends on the kind.
    pub detail: u32,
    pub pid: u16,
    pub kind: AnomalyKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub bytes: u64,
    pub packets: u64,
    /// TP_extra_header and parity bytes of the counted packets.
    pub framing_bytes: u64,
    pub null_packets: u64,
    pub scrambled_packets: u64,
    /// Packets of a PES PID outside any unit, such as before the first start.
    pub orphan_packets: u64,
    /// PAT and PMT sections that passed the CRC.
    pub sections: u64,
    /// Sections with another table_id or for an unknown program.
    pub other_sections: u64,
    pub pes_units: u64,
    pub pes_bytes: u64,
    /// Indexed by `AnomalyKind as usize`.
    pub anomalies: [u64; ANOMALY_KINDS],
}

pub fn anomaly_count(stats: &Stats, kind: AnomalyKind) -> u64 {
    stats.anomalies[kind as usize]
}

pub fn anomaly_total(stats: &Stats) -> u64 {
    stats.anomalies.iter().sum()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Program {
    /// Of a program level registration descriptor, zero when absent.
    pub registration: u32,
    pub number: u16,
    pub pmt_pid: u16,
    /// 0x1FFF when the program has no PCR or is not mapped yet.
    pub pcr_pid: u16,
    pub version: u8,
    pub mapped: bool,
    stream_count: u16,
    block: u16,
    seen_crc: u32,
    seen_version: u8,
    seen: bool,
    keep: bool,
}

pub const UF_RANDOM_ACCESS: u8 = 0x01;
/// First unit after a continuity break or a discontinuity_indicator.
pub const UF_DISCONTINUITY: u8 = 0x02;
pub const UF_DATA_ALIGNED: u8 = 0x04;
/// Packets were lost inside the unit, the payload is the part before the loss.
pub const UF_CORRUPT: u8 = 0x08;
pub const UF_TRUNCATED: u8 = 0x10;
pub const UF_SCRAMBLED: u8 = 0x20;
pub const UF_ENCRYPTED: u8 = 0x40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PesUnit<'a> {
    /// Borrowed from the pushed chunk when it fit one transport packet, else from the PID's
    /// buffer.
    pub payload: &'a [u8],
    /// 90 kHz ticks.
    pub pts: Option<u64>,
    /// 90 kHz ticks, absent means equal to the PTS.
    pub dts: Option<u64>,
    /// Of the packet the unit started in, at its TP_extra_header for 192-byte packets.
    pub offset: u64,
    /// 30-bit arrival_time_stamp in 27 MHz ticks, 192-byte packets only.
    pub arrival: Option<u32>,
    pub pid: u16,
    pub stream_id: u8,
    /// `UF_*` bits.
    pub flags: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// Programs that kept their number and map PID keep their map and get no new `Pmt` event.
    Pat {
        transport_stream_id: u16,
        version: u8,
        programs: &'a [Program],
    },
    Pmt {
        program: &'a Program,
        streams: &'a [StreamInfo],
    },
    /// PCR of a mapped program's PCR_PID, 27 MHz. `discontinuity` starts a new time base (2.4.3.5).
    Pcr {
        pid: u16,
        pcr: u64,
        discontinuity: bool,
        offset: u64,
        arrival: Option<u32>,
    },
    Pes(PesUnit<'a>),
    Anomaly(Anomaly),
}

const KIND_NONE: u8 = 0;
const KIND_PSI: u8 = 1;
const KIND_PES: u8 = 2;

const CC_UNSEEN: u8 = 0xFF;

const ATS_NONE: u32 = u32::MAX;

#[inline]
fn arrival_of(raw: u32) -> Option<u32> {
    (raw != ATS_NONE).then_some(raw)
}

#[derive(Clone, Copy)]
struct Origin {
    offset: u64,
    arrival: u32,
}

const EF_PCR: u8 = 0x01;
/// The last packet was already a duplicate.
const EF_DUP: u8 = 0x02;
const EF_SCRAMBLED: u8 = 0x04;
const EF_ON: u8 = 0x08;
const EF_OFF: u8 = 0x10;

#[derive(Clone, Copy)]
struct PidEntry {
    finger: u32,
    slot: u16,
    kind: u8,
    cc: u8,
    flags: u8,
}

const PID_ENTRY_INIT: PidEntry =
    PidEntry { finger: 0, slot: 0, kind: KIND_NONE, cc: CC_UNSEEN, flags: 0 };

/// Digest of the last twelve bytes and the payload start. A duplicate is a byte copy except the
/// PCR at the front (2.4.3.3), so equal counters with different digests mean damage.
#[inline]
fn fingerprint(raw: &[u8; PACKET_LEN], payload_start: u8) -> u32 {
    let word = |i: usize| u32::from_le_bytes([raw[i], raw[i + 1], raw[i + 2], raw[i + 3]]);
    (word(176).wrapping_mul(0x9E37_79B1) ^ word(180)).wrapping_mul(0x85EB_CA6B)
        ^ word(184)
        ^ u32::from(payload_start)
}

/// Slot zero is the PAT.
#[derive(Clone, Copy)]
struct PsiSlot {
    pid: u16,
    /// Zero when no section is in progress.
    len: u16,
    /// Valid once three bytes are in.
    need: u16,
    used: bool,
    wanted: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PesState {
    Free,
    Idle,
    /// A header that did not fit the first packet.
    Header,
    Payload,
    Skip,
}

struct PesSlot {
    buf: Vec<u8>,
    pts: Option<u64>,
    dts: Option<u64>,
    offset: u64,
    arrival: u32,
    /// Valid when `bounded`.
    remaining: u32,
    pid: u16,
    state: PesState,
    stream_id: u8,
    flags: u8,
    /// `UF_*` bits waiting for the next unit start.
    pending: u8,
    /// `UF_*` bits every unit of the PID carries.
    always: u8,
    bounded: bool,
    wanted: bool,
}

const BLOCK_NONE: u16 = u16::MAX;

pub struct Demux {
    cfg: Config,
    locked: bool,
    /// Of the current lock, or of the last one while hunting.
    size: PacketSize,
    sized: bool,
    carry_len: usize,
    carry: [u8; FRAMED_MAX],
    /// `sync_confirm` packets of the largest size after up to one TP_extra_header already passed.
    hunt: Vec<u8>,
    hunt_at: usize,
    resync_left: u32,
    arrival: u32,
    pos: u64,
    /// Dropped since alignment was lost, not reported yet.
    skipped: u64,
    pids: Vec<PidEntry>,
    /// So a rebuild need not sweep the whole PID table.
    routed: Vec<u16>,
    map_dirty: bool,
    psi_slots: Vec<PsiSlot>,
    psi_buf: Vec<u8>,
    pat_seen: bool,
    pat_version: u8,
    pat_crc: u32,
    transport_stream_id: u16,
    network_pid: u16,
    programs: Vec<Program>,
    programs_next: Vec<Program>,
    /// `max_streams_per_program` rows per program, indexed by `Program::block`.
    streams: Vec<StreamInfo>,
    /// A program's map before a new version, PID and stream type of each stream.
    was: Vec<(u16, u8)>,
    free_blocks: Vec<u16>,
    pes: Vec<PesSlot>,
    stats: Stats,
}

#[inline]
fn note<F: FnMut(Event<'_>)>(
    stats: &mut Stats,
    sink: &mut F,
    kind: AnomalyKind,
    pid: u16,
    offset: u64,
    detail: u32,
) {
    stats.anomalies[kind as usize] += 1;
    sink(Event::Anomaly(Anomaly { offset, detail, pid, kind }));
}

#[inline]
fn emit_unit<F: FnMut(Event<'_>)>(
    s: &PesSlot,
    payload: &[u8],
    extra: u8,
    stats: &mut Stats,
    sink: &mut F,
) {
    stats.pes_units += 1;
    stats.pes_bytes += payload.len() as u64;
    sink(Event::Pes(PesUnit {
        payload,
        pts: s.pts,
        dts: s.dts,
        offset: s.offset,
        arrival: arrival_of(s.arrival),
        pid: s.pid,
        stream_id: s.stream_id,
        flags: s.flags | s.always | extra,
    }));
}

enum Grow {
    TooLarge,
    Alloc,
}

/// Never grows capacity past `max`.
#[inline]
fn push_bytes(buf: &mut Vec<u8>, data: &[u8], max: usize) -> Result<(), Grow> {
    let need = buf.len() + data.len();
    if need > max {
        return Err(Grow::TooLarge);
    }
    if need > buf.capacity() {
        let target = need.max(buf.capacity().saturating_mul(2)).min(max);
        if buf.try_reserve_exact(target - buf.len()).is_err() {
            return Err(Grow::Alloc);
        }
    }
    buf.extend_from_slice(data);
    Ok(())
}

fn apply_header<F: FnMut(Event<'_>)>(
    s: &mut PesSlot,
    h: &PesHeader,
    stats: &mut Stats,
    sink: &mut F,
) {
    s.stream_id = h.stream_id;
    s.pts = h.pts;
    s.dts = h.dts;
    s.bounded = h.payload_len.is_some();
    s.remaining = h.payload_len.unwrap_or(0);
    if h.flags & PH_DATA_ALIGNED != 0 {
        s.flags |= UF_DATA_ALIGNED;
    }
    if h.flags & PH_SCRAMBLED != 0 {
        s.flags |= UF_SCRAMBLED;
    }
    if h.flags & PH_BAD_TIMESTAMP != 0 {
        note(stats, sink, AnomalyKind::PesTimestamp, s.pid, s.offset, 0);
    }
}

fn pes_error_kind(e: PesError) -> AnomalyKind {
    match e {
        PesError::StartCode => AnomalyKind::PesStartCode,
        PesError::Syntax | PesError::Length => AnomalyKind::PesHeader,
    }
}

fn pes_drop<F: FnMut(Event<'_>)>(
    s: &mut PesSlot,
    why: Grow,
    max: usize,
    stats: &mut Stats,
    sink: &mut F,
) {
    let (kind, detail) = match why {
        Grow::TooLarge => (AnomalyKind::PesTooLarge, max as u32),
        Grow::Alloc => (AnomalyKind::PesAlloc, 0),
    };
    note(stats, sink, kind, s.pid, s.offset, detail);
    s.buf.clear();
    s.state = PesState::Skip;
}

fn pes_first_body<F: FnMut(Event<'_>)>(
    s: &mut PesSlot,
    h: &PesHeader,
    body: &[u8],
    max: usize,
    stats: &mut Stats,
    sink: &mut F,
) {
    if h.flags & PH_PADDING != 0 {
        s.state = PesState::Skip;
        return;
    }
    match h.payload_len {
        Some(n) => {
            let n = n as usize;
            if body.len() >= n {
                // The whole PES packet sits in this transport packet, no copy.
                emit_unit(s, &body[..n], 0, stats, sink);
                s.state = PesState::Idle;
            } else if n > max {
                note(stats, sink, AnomalyKind::PesTooLarge, s.pid, s.offset, n as u32);
                s.state = PesState::Skip;
            } else {
                match push_bytes(&mut s.buf, body, max) {
                    Ok(()) => {
                        s.remaining = (n - body.len()) as u32;
                        s.state = PesState::Payload;
                    }
                    Err(why) => pes_drop(s, why, max, stats, sink),
                }
            }
        }
        None => match push_bytes(&mut s.buf, body, max) {
            Ok(()) => s.state = PesState::Payload,
            Err(why) => pes_drop(s, why, max, stats, sink),
        },
    }
}

fn pes_header_more<F: FnMut(Event<'_>)>(
    s: &mut PesSlot,
    data: &[u8],
    max: usize,
    stats: &mut Stats,
    sink: &mut F,
) {
    if let Err(why) = push_bytes(&mut s.buf, data, max) {
        pes_drop(s, why, max, stats, sink);
        return;
    }
    let h = match parse_pes_header(&s.buf) {
        Ok(Some(h)) => h,
        Ok(None) => return,
        Err(e) => {
            note(stats, sink, pes_error_kind(e), s.pid, s.offset, 0);
            s.buf.clear();
            s.state = PesState::Skip;
            return;
        }
    };
    apply_header(s, &h, stats, sink);
    s.buf.drain(..usize::from(h.header_len).min(s.buf.len()));
    if h.flags & PH_PADDING != 0 {
        s.buf.clear();
        s.state = PesState::Skip;
        return;
    }
    s.state = PesState::Payload;
    if let Some(n) = h.payload_len {
        let n = n as usize;
        if s.buf.len() >= n {
            s.buf.truncate(n);
            emit_unit(s, &s.buf, 0, stats, sink);
            s.buf.clear();
            s.state = PesState::Idle;
        } else if n > max {
            note(stats, sink, AnomalyKind::PesTooLarge, s.pid, s.offset, n as u32);
            s.buf.clear();
            s.state = PesState::Skip;
        } else {
            s.remaining = (n - s.buf.len()) as u32;
        }
    }
}

fn pes_packet<F: FnMut(Event<'_>)>(
    s: &mut PesSlot,
    pkt: &Packet,
    data: &[u8],
    origin: Origin,
    max: usize,
    stats: &mut Stats,
    sink: &mut F,
) {
    let offset = origin.offset;
    if pkt.flags & PF_UNIT_START != 0 {
        // H.222.0 2.4.3.3, exactly one PES packet starts here, so the previous one ends.
        match s.state {
            PesState::Payload => {
                let mut extra = 0;
                if s.bounded {
                    note(stats, sink, AnomalyKind::PesTruncated, s.pid, s.offset, s.remaining);
                    extra = UF_TRUNCATED;
                }
                emit_unit(s, &s.buf, extra, stats, sink);
            }
            PesState::Header => note(stats, sink, AnomalyKind::PesHeader, s.pid, s.offset, 0),
            _ => {}
        }
        s.buf.clear();
        s.flags = s.pending;
        s.pending = 0;
        s.offset = offset;
        s.arrival = origin.arrival;
        match parse_pes_header(data) {
            Ok(Some(h)) => {
                apply_header(s, &h, stats, sink);
                let body = data.get(usize::from(h.header_len)..).unwrap_or(&[]);
                pes_first_body(s, &h, body, max, stats, sink);
            }
            Ok(None) => match push_bytes(&mut s.buf, data, max) {
                Ok(()) => s.state = PesState::Header,
                Err(why) => pes_drop(s, why, max, stats, sink),
            },
            Err(e) => {
                note(stats, sink, pes_error_kind(e), s.pid, offset, 0);
                s.state = PesState::Skip;
            }
        }
        return;
    }
    match s.state {
        PesState::Payload => {
            let take = if s.bounded { data.len().min(s.remaining as usize) } else { data.len() };
            if let Err(why) = push_bytes(&mut s.buf, &data[..take], max) {
                pes_drop(s, why, max, stats, sink);
                return;
            }
            if s.bounded {
                s.remaining -= take as u32;
                if s.remaining == 0 {
                    emit_unit(s, &s.buf, 0, stats, sink);
                    s.buf.clear();
                    s.state = PesState::Idle;
                }
            }
        }
        PesState::Header => pes_header_more(s, data, max, stats, sink),
        _ => stats.orphan_packets += 1,
    }
}

fn pes_break<F: FnMut(Event<'_>)>(s: &mut PesSlot, stats: &mut Stats, sink: &mut F) {
    match s.state {
        PesState::Payload => {
            if !s.buf.is_empty() {
                emit_unit(s, &s.buf, UF_CORRUPT, stats, sink);
            }
            s.buf.clear();
            s.state = PesState::Skip;
        }
        PesState::Header => {
            s.buf.clear();
            s.state = PesState::Skip;
        }
        _ => {}
    }
    s.pending |= UF_DISCONTINUITY;
}

impl Demux {
    #[allow(clippy::disallowed_methods)] // one-time setup, the steady state never allocates
    pub fn new(cfg: Config) -> Result<Demux, ConfigError> {
        let l = cfg.limits;
        if !(1..=1024).contains(&l.max_programs) {
            return Err(ConfigError::Programs);
        }
        if !(1..=256).contains(&l.max_streams_per_program) {
            return Err(ConfigError::StreamsPerProgram);
        }
        if !(1..=PID_COUNT).contains(&usize::from(l.max_pes_pids)) {
            return Err(ConfigError::PesPids);
        }
        if !(MIN_PES_BYTES..=1 << 30).contains(&l.max_pes_bytes) {
            return Err(ConfigError::PesBytes);
        }
        if l.pes_reserve_bytes > l.max_pes_bytes {
            return Err(ConfigError::PesReserve);
        }
        if !(64..=SECTION_MAX).contains(&usize::from(l.max_section_bytes)) {
            return Err(ConfigError::SectionBytes);
        }
        if !(1..=SYNC_CONFIRM_MAX).contains(&l.sync_confirm) {
            return Err(ConfigError::SyncConfirm);
        }
        let programs = usize::from(l.max_programs);
        let streams = usize::from(l.max_streams_per_program);
        let psi_slots = programs + 1;
        let blank_stream = stream_info(0, PID_NULL, &[]);
        let mut pes = Vec::with_capacity(usize::from(l.max_pes_pids));
        for _ in 0..l.max_pes_pids {
            pes.push(PesSlot {
                buf: Vec::new(),
                pts: None,
                dts: None,
                offset: 0,
                arrival: ATS_NONE,
                remaining: 0,
                pid: PID_NULL,
                state: PesState::Free,
                stream_id: 0,
                flags: 0,
                pending: 0,
                always: 0,
                bounded: false,
                wanted: false,
            });
        }
        let mut d = Demux {
            cfg,
            locked: false,
            size: PacketSize::Ts188,
            sized: false,
            carry_len: 0,
            carry: [0; FRAMED_MAX],
            hunt: Vec::with_capacity(usize::from(l.sync_confirm) * FRAMED_MAX + EXTRA_HEADER_LEN),
            hunt_at: 0,
            resync_left: 0,
            arrival: ATS_NONE,
            pos: 0,
            skipped: 0,
            pids: vec![PID_ENTRY_INIT; PID_COUNT],
            // The PAT, a map PID and a PCR PID per program, and the PES PIDs.
            routed: Vec::with_capacity(1 + 2 * programs + usize::from(l.max_pes_pids)),
            map_dirty: false,
            psi_slots: vec![
                PsiSlot { pid: PID_NULL, len: 0, need: 0, used: false, wanted: false };
                psi_slots
            ],
            psi_buf: vec![0; psi_slots * usize::from(l.max_section_bytes)],
            pat_seen: false,
            pat_version: 0,
            pat_crc: 0,
            transport_stream_id: 0,
            network_pid: PID_NULL,
            programs: Vec::with_capacity(programs),
            programs_next: Vec::with_capacity(programs),
            streams: vec![blank_stream; programs * streams],
            was: Vec::with_capacity(streams),
            free_blocks: Vec::with_capacity(programs),
            pes,
            stats: Stats::default(),
        };
        d.reset();
        Ok(d)
    }

    /// Back to the state after [`Demux::new`], keeping all memory.
    pub fn reset(&mut self) {
        self.locked = false;
        self.size = PacketSize::Ts188;
        self.sized = false;
        self.carry_len = 0;
        self.hunt.clear();
        self.hunt_at = 0;
        self.resync_left = 0;
        self.skipped = 0;
        self.pos = 0;
        self.pids.fill(PID_ENTRY_INIT);
        self.routed.clear();
        for s in &mut self.psi_slots {
            s.used = false;
            s.len = 0;
        }
        self.pat_seen = false;
        self.transport_stream_id = 0;
        self.network_pid = PID_NULL;
        self.programs.clear();
        self.programs_next.clear();
        self.free_blocks.clear();
        for block in (0..self.cfg.limits.max_programs).rev() {
            self.free_blocks.push(block);
        }
        for s in &mut self.pes {
            s.state = PesState::Free;
            s.buf.clear();
            s.pending = 0;
        }
        self.stats = Stats::default();
        self.map_dirty = false;
        if let (Some(e), Some(s)) =
            (self.pids.get_mut(usize::from(PID_PAT)), self.psi_slots.get_mut(0))
        {
            e.kind = KIND_PSI;
            e.slot = 0;
            s.used = true;
            s.pid = PID_PAT;
            self.routed.push(PID_PAT);
        }
    }

    /// The next bytes do not follow the previous ones. Partial state is dropped silently, the next
    /// unit per PID carries [`UF_DISCONTINUITY`], tables and offset are kept, every size is tried.
    pub fn discontinuity(&mut self) {
        self.locked = false;
        self.carry_len = 0;
        self.hunt.clear();
        self.hunt_at = 0;
        self.resync_left = 0;
        self.skipped = 0;
        for e in &mut self.pids {
            e.cc = CC_UNSEEN;
            e.flags &= !(EF_DUP | EF_SCRAMBLED);
        }
        for s in &mut self.psi_slots {
            s.len = 0;
        }
        for s in &mut self.pes {
            if s.state != PesState::Free {
                s.state = PesState::Idle;
                s.buf.clear();
                s.pending = UF_DISCONTINUITY;
            }
        }
    }

    pub fn set_position(&mut self, offset: u64) {
        self.pos = offset;
    }

    /// Offset of the first byte not yet consumed as a packet.
    pub fn position(&self) -> u64 {
        self.pos
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Of the current lock or the last one while searching, `None` before the first lock.
    pub fn packet_size(&self) -> Option<PacketSize> {
        self.sized.then_some(self.size)
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn programs(&self) -> &[Program] {
        &self.programs
    }

    pub fn streams(&self, program: &Program) -> &[StreamInfo] {
        let per = usize::from(self.cfg.limits.max_streams_per_program);
        let start = usize::from(program.block) * per;
        self.streams.get(start..start + usize::from(program.stream_count).min(per)).unwrap_or(&[])
    }

    pub fn transport_stream_id(&self) -> Option<u16> {
        self.pat_seen.then_some(self.transport_stream_id)
    }

    pub fn network_pid(&self) -> Option<u16> {
        (self.network_pid != PID_NULL).then_some(self.network_pid)
    }

    pub fn pes_buffer_capacity(&self) -> usize {
        self.pes.iter().map(|s| s.buf.capacity()).max().unwrap_or(0)
    }

    pub fn pes_pids(&self) -> usize {
        self.pes.iter().filter(|s| s.state != PesState::Free).count()
    }

    /// Overrides [`Config::select_all`] from the next push or flush. The only way to get the units
    /// of an encrypted stream.
    pub fn select(&mut self, pid: u16, on: bool) {
        if let Some(e) = self.pids.get_mut(usize::from(pid)) {
            e.flags = (e.flags & !(EF_ON | EF_OFF)) | if on { EF_ON } else { EF_OFF };
            self.map_dirty = true;
        }
    }

    /// Chunk boundaries never change the events. `sink` gets each event in stream order.
    pub fn push<F: FnMut(Event<'_>)>(&mut self, mut data: &[u8], sink: &mut F) {
        if self.map_dirty {
            self.rebuild(self.pos, sink);
        }
        self.stats.bytes += data.len() as u64;
        loop {
            if !self.locked {
                data = self.hunt(data, sink);
                if !self.locked {
                    return;
                }
            }
            if self.carry_len > 0 && !self.finish_carry(&mut data, sink) {
                if self.locked {
                    return;
                }
                continue;
            }
            if self.size == PacketSize::Ts188 {
                while let Some((raw, rest)) = data.split_first_chunk::<PACKET_LEN>() {
                    if raw[0] != SYNC_BYTE {
                        self.lose_sync(sink);
                        break;
                    }
                    let offset = self.pos;
                    self.pos += PACKET_LEN as u64;
                    data = rest;
                    self.packet(raw, offset, sink);
                }
            } else {
                data = self.framed(data, sink);
            }
            if self.locked {
                if let Some(carry) = self.carry.get_mut(..data.len()) {
                    carry.copy_from_slice(data);
                    self.carry_len = data.len();
                }
                return;
            }
        }
    }

    /// False when the input ran out first or the packet lost the alignment.
    fn finish_carry<F: FnMut(Event<'_>)>(&mut self, data: &mut &[u8], sink: &mut F) -> bool {
        let len = framed_len(self.size);
        let have = self.carry_len.min(len);
        let (head, rest) = data.split_at(data.len().min(len - have));
        if let Some(room) = self.carry.get_mut(have..have + head.len()) {
            room.copy_from_slice(head);
        }
        *data = rest;
        self.carry_len = have + head.len();
        if self.carry_len < len {
            return false;
        }
        self.carry_len = 0;
        let unit = self.carry;
        let unit = unit.get(..len).unwrap_or(&[]);
        if self.unit(unit, sink) {
            return true;
        }
        self.lose_sync(sink);
        self.hunt.extend_from_slice(unit);
        false
    }

    /// Returns the leftover input, starting at the packet that lost alignment if one did.
    fn framed<'d, F: FnMut(Event<'_>)>(&mut self, mut data: &'d [u8], sink: &mut F) -> &'d [u8] {
        let len = framed_len(self.size);
        while let Some((unit, rest)) = data.split_at_checked(len) {
            if !self.unit(unit, sink) {
                self.lose_sync(sink);
                break;
            }
            data = rest;
        }
        data
    }

    /// False, consuming nothing, when it lacks the sync byte.
    #[inline]
    fn unit<F: FnMut(Event<'_>)>(&mut self, unit: &[u8], sink: &mut F) -> bool {
        let raw = unit.get(sync_offset(self.size)..).and_then(|u| u.first_chunk::<PACKET_LEN>());
        let Some(raw) = raw.filter(|raw| raw[0] == SYNC_BYTE) else { return false };
        if self.size == PacketSize::M2ts192 {
            self.arrival =
                unit.first_chunk::<EXTRA_HEADER_LEN>().map_or(ATS_NONE, arrival_time_stamp);
        }
        let offset = self.pos;
        self.pos += unit.len() as u64;
        self.stats.framing_bytes += (unit.len() - PACKET_LEN) as u64;
        self.packet(raw, offset, sink);
        true
    }

    /// Delivers pending units, counts leftover bytes and resets as [`Demux::discontinuity`] does.
    pub fn flush<F: FnMut(Event<'_>)>(&mut self, sink: &mut F) {
        if self.map_dirty {
            self.rebuild(self.pos, sink);
        }
        let leftover = if self.locked { self.carry_len } else { self.hunt.len() } as u64;
        let dropped = self.skipped + leftover;
        if dropped > 0 {
            let detail = dropped.min(u64::from(u32::MAX)) as u32;
            note(&mut self.stats, sink, AnomalyKind::SyncSkipped, PID_NULL, self.pos, detail);
        }
        self.pos += leftover;
        for s in &mut self.pes {
            if s.state == PesState::Payload {
                let mut extra = 0;
                if s.bounded {
                    note(
                        &mut self.stats,
                        sink,
                        AnomalyKind::PesTruncated,
                        s.pid,
                        s.offset,
                        s.remaining,
                    );
                    extra = UF_TRUNCATED;
                }
                emit_unit(s, &s.buf, extra, &mut self.stats, sink);
            } else if s.state == PesState::Header {
                note(&mut self.stats, sink, AnomalyKind::PesHeader, s.pid, s.offset, 0);
            }
        }
        self.discontinuity();
    }

    fn lose_sync<F: FnMut(Event<'_>)>(&mut self, sink: &mut F) {
        note(&mut self.stats, sink, AnomalyKind::SyncLost, PID_NULL, self.pos, 0);
        self.locked = false;
        self.skipped = 0;
        self.hunt_at = 0;
        self.resync_left = RESYNC_BUDGET;
    }

    /// A search that finds nothing still reports once per [`RESYNC_BUDGET`] bytes.
    fn skip<F: FnMut(Event<'_>)>(&mut self, n: usize, sink: &mut F) {
        self.pos += n as u64;
        self.skipped += n as u64;
        // `n` is at most one search window, which is smaller than the budget.
        if self.skipped >= u64::from(RESYNC_BUDGET) {
            self.skipped -= u64::from(RESYNC_BUDGET);
            let offset = self.pos - self.skipped;
            note(&mut self.stats, sink, AnomalyKind::SyncSkipped, PID_NULL, offset, RESYNC_BUDGET);
        }
    }

    /// Returns unconsumed input. After a loss only the lost size is tried for [`RESYNC_BUDGET`]
    /// positions so damage cannot flip it, then every size, which picks up a real size change.
    fn hunt<'d, F: FnMut(Event<'_>)>(&mut self, mut data: &'d [u8], sink: &mut F) -> &'d [u8] {
        let confirm = usize::from(self.cfg.limits.sync_confirm);
        loop {
            let room = self.hunt.capacity().saturating_sub(self.hunt.len());
            let (head, rest) = data.split_at(room.min(data.len()));
            self.hunt.extend_from_slice(head);
            data = rest;
            let resync = self.resync_left > 0;
            let (allowed, limit) = if resync {
                (only(self.size), self.resync_left as usize)
            } else {
                (ANY_SIZE, usize::MAX)
            };
            let found = scan(&self.hunt, self.hunt_at, confirm, allowed, limit);
            let judged = found.at.saturating_sub(self.hunt_at);
            self.resync_left = self.resync_left.saturating_sub(judged as u32);
            if let Some(size) = found.size {
                if self.lock(size, found.at.saturating_sub(sync_offset(size)), sink) {
                    return data;
                }
                continue;
            }
            // The last bytes passed over stay, they may be the header of a 192-byte packet.
            let drop = found.at.saturating_sub(EXTRA_HEADER_LEN);
            self.hunt.drain(..drop);
            self.hunt_at = found.at - drop;
            self.skip(drop, sink);
            // Judge again when the candidate set widened, or when new input fits.
            let widened = resync && self.resync_left == 0;
            if !widened && (data.is_empty() || judged == 0) {
                return data;
            }
        }
    }

    /// False when a packet in the window lacks its sync byte, the search resumes there.
    fn lock<F: FnMut(Event<'_>)>(&mut self, size: PacketSize, start: usize, sink: &mut F) -> bool {
        self.skip(start, sink);
        if self.skipped > 0 {
            let detail = self.skipped.min(u64::from(u32::MAX)) as u32;
            note(&mut self.stats, sink, AnomalyKind::SyncSkipped, PID_NULL, self.pos, detail);
            self.skipped = 0;
        }
        if self.sized && size != self.size {
            let detail = framed_len(size) as u32;
            note(&mut self.stats, sink, AnomalyKind::PacketSizeChanged, PID_NULL, self.pos, detail);
        }
        self.size = size;
        self.sized = true;
        self.locked = true;
        self.resync_left = 0;
        self.arrival = ATS_NONE;
        let len = framed_len(size);
        let window = std::mem::take(&mut self.hunt);
        let mut used = start;
        let mut kept = true;
        while let Some(unit) = window.get(used..used + len) {
            if !self.unit(unit, sink) {
                kept = false;
                break;
            }
            used += len;
        }
        self.hunt = window;
        self.hunt_at = 0;
        if kept {
            let rest = self.hunt.get(used..).unwrap_or(&[]);
            if let Some(carry) = self.carry.get_mut(..rest.len()) {
                carry.copy_from_slice(rest);
                self.carry_len = rest.len();
            }
            self.hunt.clear();
        } else {
            self.lose_sync(sink);
            self.hunt.drain(..used.min(self.hunt.len()));
        }
        kept
    }

    #[inline]
    fn packet<F: FnMut(Event<'_>)>(&mut self, raw: &[u8; PACKET_LEN], offset: u64, sink: &mut F) {
        self.stats.packets += 1;
        let pkt = match parse_packet(raw) {
            Ok(pkt) => pkt,
            Err(e) => {
                let (kind, detail) = match e {
                    PacketError::Sync => (AnomalyKind::SyncLost, 0),
                    PacketError::TransportError => (AnomalyKind::TransportError, 0),
                    PacketError::ReservedControl => (AnomalyKind::AdaptationField, 0),
                    PacketError::AdaptationLength => {
                        (AnomalyKind::AdaptationField, u32::from(raw[4]))
                    }
                };
                note(&mut self.stats, sink, kind, pid_of(raw), offset, detail);
                return;
            }
        };
        // H.222.0 2.4.3.3, null packets have no defined continuity or payload.
        if pkt.pid == PID_NULL {
            self.stats.null_packets += 1;
            return;
        }
        let index = usize::from(pkt.pid);
        let Some(&entry) = self.pids.get(index) else { return };
        let mut e = entry;
        let slot = usize::from(e.slot);
        let max_pes = self.cfg.limits.max_pes_bytes as usize;

        if pkt.flags & (PF_PCR | PF_PCR_INVALID) != 0 {
            if pkt.flags & PF_PCR_INVALID != 0 {
                note(&mut self.stats, sink, AnomalyKind::PcrInvalid, pkt.pid, offset, 0);
            } else if e.flags & EF_PCR != 0 {
                sink(Event::Pcr {
                    pid: pkt.pid,
                    pcr: pkt.pcr,
                    discontinuity: pkt.flags & PF_DISCONTINUITY != 0,
                    offset,
                    arrival: arrival_of(self.arrival),
                });
            }
        }
        // Both describe the next PES packet on the PID (2.4.3.5), even on a packet without payload.
        if e.kind == KIND_PES
            && pkt.flags & (PF_DISCONTINUITY | PF_RANDOM_ACCESS) != 0
            && let Some(s) = self.pes.get_mut(slot)
        {
            if pkt.flags & PF_RANDOM_ACCESS != 0 {
                s.pending |= UF_RANDOM_ACCESS;
            }
            if pkt.flags & PF_DISCONTINUITY != 0 {
                s.pending |= UF_DISCONTINUITY;
            }
        }
        // The counter does not advance without payload (2.4.3.3), nothing left to do.
        if pkt.flags & PF_PAYLOAD == 0 {
            return;
        }

        let finger = fingerprint(raw, pkt.payload_start);
        let mut broke = false;
        if e.cc != CC_UNSEEN {
            let expected = (e.cc + 1) & 0x0F;
            if pkt.continuity == expected {
                e.flags &= !EF_DUP;
            } else if pkt.continuity == e.cc && e.flags & EF_DUP == 0 && finger == e.finger {
                // Sent at most twice (2.4.3.3), the copy carries nothing new.
                e.flags |= EF_DUP;
                self.pids[index] = e;
                note(&mut self.stats, sink, AnomalyKind::Duplicate, pkt.pid, offset, 0);
                return;
            } else {
                e.flags &= !EF_DUP;
                broke = true;
                // A jump announced by discontinuity_indicator is legal, not a loss.
                if pkt.flags & PF_DISCONTINUITY == 0 {
                    let detail = (u32::from(expected) << 8) | u32::from(pkt.continuity);
                    note(
                        &mut self.stats,
                        sink,
                        AnomalyKind::ContinuityLoss,
                        pkt.pid,
                        offset,
                        detail,
                    );
                }
            }
        }
        e.cc = pkt.continuity;
        e.finger = finger;

        let scrambled = pkt.scrambling != 0;
        if scrambled {
            self.stats.scrambled_packets += 1;
            if e.flags & EF_SCRAMBLED == 0 {
                e.flags |= EF_SCRAMBLED;
                let detail = u32::from(pkt.scrambling);
                note(&mut self.stats, sink, AnomalyKind::Scrambled, pkt.pid, offset, detail);
            }
        } else {
            e.flags &= !EF_SCRAMBLED;
        }
        self.pids[index] = e;

        match e.kind {
            KIND_PES => {
                let Some(s) = self.pes.get_mut(slot) else { return };
                if broke || scrambled {
                    pes_break(s, &mut self.stats, sink);
                }
                if !scrambled {
                    let origin = Origin { offset, arrival: self.arrival };
                    pes_packet(s, &pkt, payload(raw, &pkt), origin, max_pes, &mut self.stats, sink);
                }
            }
            KIND_PSI => {
                if (broke || scrambled)
                    && let Some(s) = self.psi_slots.get_mut(slot)
                {
                    s.len = 0;
                }
                if !scrambled {
                    self.psi_packet(slot, &pkt, payload(raw, &pkt), offset, sink);
                }
            }
            _ => {}
        }
    }

    fn psi_packet<F: FnMut(Event<'_>)>(
        &mut self,
        slot: usize,
        pkt: &Packet,
        mut data: &[u8],
        offset: u64,
        sink: &mut F,
    ) {
        let max = usize::from(self.cfg.limits.max_section_bytes);
        // The section bytes live outside `self` while tables are updated from them.
        let mut buf = std::mem::take(&mut self.psi_buf);
        'feed: {
            let Some(area) = buf.get_mut(slot * max..(slot + 1) * max) else { break 'feed };
            let Some(pid) = self.psi_slots.get(slot).map(|s| s.pid) else { break 'feed };
            if pkt.flags & PF_UNIT_START == 0 {
                if self.psi_slots[slot].len > 0 {
                    self.section_bytes(slot, area, data, offset, sink);
                }
                break 'feed;
            }
            // 2.4.4.2, pointer_field counts the bytes before the first section start.
            let Some((&pointer, rest)) = data.split_first() else {
                note(&mut self.stats, sink, AnomalyKind::PsiPointer, pid, offset, 0);
                break 'feed;
            };
            let pointer = usize::from(pointer);
            if pointer > rest.len() {
                note(&mut self.stats, sink, AnomalyKind::PsiPointer, pid, offset, pointer as u32);
                self.psi_slots[slot].len = 0;
                break 'feed;
            }
            let (tail, starts) = rest.split_at(pointer);
            if self.psi_slots[slot].len > 0 {
                self.section_bytes(slot, area, tail, offset, sink);
            }
            self.psi_slots[slot].len = 0;
            data = starts;
            // Sections follow back to back until stuffing (2.4.4) or the packet end.
            while let Some(&table_id) = data.first() {
                if table_id == 0xFF {
                    break;
                }
                match self.section_bytes(slot, area, data, offset, sink) {
                    Some(used) if used > 0 => data = &data[used.min(data.len())..],
                    _ => break,
                }
            }
        }
        self.psi_buf = buf;
    }

    /// Returns the bytes consumed, `None` to skip the rest of the packet.
    fn section_bytes<F: FnMut(Event<'_>)>(
        &mut self,
        slot: usize,
        area: &mut [u8],
        data: &[u8],
        offset: u64,
        sink: &mut F,
    ) -> Option<usize> {
        let s = self.psi_slots.get_mut(slot)?;
        let mut len = usize::from(s.len);
        let mut need = usize::from(s.need);
        let mut used = 0;
        if len < 3 {
            let n = (3 - len).min(data.len());
            area.get_mut(len..len + n)?.copy_from_slice(&data[..n]);
            len += n;
            used = n;
            if len < 3 {
                s.len = len as u16;
                return Some(used);
            }
            need = section_total_len(area[1], area[2]);
            if need > area.len() {
                s.len = 0;
                let pid = s.pid;
                note(&mut self.stats, sink, AnomalyKind::SectionTooLong, pid, offset, need as u32);
                return None;
            }
            s.need = need as u16;
        }
        let n = need.saturating_sub(len).min(data.len() - used);
        area.get_mut(len..len + n)?.copy_from_slice(&data[used..used + n]);
        len += n;
        used += n;
        if len < need {
            s.len = len as u16;
            return Some(used);
        }
        s.len = 0;
        let pid = s.pid;
        self.section(slot, pid, area.get(..need)?, offset, sink);
        Some(used)
    }

    fn section<F: FnMut(Event<'_>)>(
        &mut self,
        slot: usize,
        pid: u16,
        sec: &[u8],
        offset: u64,
        sink: &mut F,
    ) {
        // Only table_id 0 on PID 0, a map PID may also carry private sections (2.4.4, 2.4.4.10).
        let expect = if slot == 0 { TABLE_ID_PAT } else { TABLE_ID_PMT };
        if sec.first() != Some(&expect) {
            self.stats.other_sections += 1;
            return;
        }
        let h = match check_section(sec) {
            Ok(h) => h,
            Err(PsiError::Crc) => {
                note(
                    &mut self.stats,
                    sink,
                    AnomalyKind::SectionCrc,
                    pid,
                    offset,
                    u32::from(expect),
                );
                return;
            }
            Err(_) => {
                note(&mut self.stats, sink, AnomalyKind::PsiMalformed, pid, offset, 0);
                return;
            }
        };
        self.stats.sections += 1;
        // 2.4.4.5 and 2.4.4.9, a table with current_next_indicator 0 is not yet applicable.
        if !h.current {
            return;
        }
        if slot == 0 {
            self.pat(sec, &h, offset, sink);
        } else {
            self.pmt(sec, &h, pid, offset, sink);
        }
    }

    /// H.222.0 2.4.4.3.
    fn pat<F: FnMut(Event<'_>)>(
        &mut self,
        sec: &[u8],
        h: &SectionHeader,
        offset: u64,
        sink: &mut F,
    ) {
        if h.section_number != 0 {
            let detail = u32::from(h.section_number);
            note(&mut self.stats, sink, AnomalyKind::PatSectionIgnored, PID_PAT, offset, detail);
            return;
        }
        // The CRC is compared too, some sources change the table without a new version.
        if self.pat_seen && self.pat_version == h.version && self.pat_crc == h.crc {
            return;
        }
        let Ok(rows) = pat_rows(sec) else {
            note(&mut self.stats, sink, AnomalyKind::PsiMalformed, PID_PAT, offset, 0);
            return;
        };
        self.pat_seen = true;
        self.pat_version = h.version;
        self.pat_crc = h.crc;
        self.transport_stream_id = h.id;

        // Programs keeping number and map PID keep their map, the rest free their block first.
        for old in &mut self.programs {
            old.keep = rows
                .iter()
                .map(pat_entry)
                .any(|e| e.program_number == old.number && e.pid == old.pmt_pid);
            if !old.keep {
                self.free_blocks.push(old.block);
                old.block = BLOCK_NONE;
            }
        }
        let max = usize::from(self.cfg.limits.max_programs);
        let mut network_pid = PID_NULL;
        let mut ignored = 0u32;
        self.programs_next.clear();
        for e in rows.iter().map(pat_entry) {
            if e.program_number == 0 {
                network_pid = e.pid;
                continue;
            }
            // 2.4.4.5, map PIDs are assignable and a program_number appears once per table.
            if !pid_is_assignable(e.pid)
                || self.programs_next.iter().any(|p| p.number == e.program_number)
            {
                note(
                    &mut self.stats,
                    sink,
                    AnomalyKind::PsiMalformed,
                    e.pid,
                    offset,
                    u32::from(e.program_number),
                );
                continue;
            }
            if self.programs_next.len() >= max {
                ignored += 1;
                continue;
            }
            let kept = self
                .programs
                .iter_mut()
                .find(|p| p.keep && p.number == e.program_number && p.pmt_pid == e.pid);
            let program = match kept {
                Some(old) => {
                    old.keep = false;
                    *old
                }
                None => {
                    let Some(block) = self.free_blocks.pop() else {
                        ignored += 1;
                        continue;
                    };
                    Program {
                        registration: 0,
                        number: e.program_number,
                        pmt_pid: e.pid,
                        pcr_pid: PID_NULL,
                        version: 0,
                        mapped: false,
                        stream_count: 0,
                        block,
                        seen_crc: 0,
                        seen_version: 0,
                        seen: false,
                        keep: false,
                    }
                }
            };
            self.programs_next.push(program);
        }
        // Kept programs that the limit pushed out after all.
        for old in &self.programs {
            if old.keep {
                self.free_blocks.push(old.block);
            }
        }
        std::mem::swap(&mut self.programs, &mut self.programs_next);
        self.network_pid = network_pid;
        if ignored > 0 {
            note(&mut self.stats, sink, AnomalyKind::ProgramLimit, PID_PAT, offset, ignored);
        }
        self.rebuild(offset, sink);
        sink(Event::Pat {
            transport_stream_id: h.id,
            version: h.version,
            programs: &self.programs,
        });
    }

    /// H.222.0 2.4.4.8.
    fn pmt<F: FnMut(Event<'_>)>(
        &mut self,
        sec: &[u8],
        h: &SectionHeader,
        pid: u16,
        offset: u64,
        sink: &mut F,
    ) {
        let Some(index) = self.programs.iter().position(|p| p.number == h.id && p.pmt_pid == pid)
        else {
            self.stats.other_sections += 1;
            return;
        };
        let p = &mut self.programs[index];
        if p.seen && p.seen_version == h.version && p.seen_crc == h.crc {
            return;
        }
        // Remembered even when rejected below, so a broken table is reported once.
        p.seen = true;
        p.seen_version = h.version;
        p.seen_crc = h.crc;
        // 2.4.4.9, a program definition is a single section numbered zero.
        let head = match pmt_head(sec) {
            Ok(head) if h.section_number == 0 => head,
            _ => {
                note(
                    &mut self.stats,
                    sink,
                    AnomalyKind::PsiMalformed,
                    pid,
                    offset,
                    u32::from(h.id),
                );
                return;
            }
        };
        let per = usize::from(self.cfg.limits.max_streams_per_program);
        let start = usize::from(p.block) * per;
        let Some(area) = self.streams.get_mut(start..start + per) else { return };
        self.was.clear();
        if p.mapped {
            let old = &area[..usize::from(p.stream_count).min(per)];
            self.was.extend(old.iter().map(|s| (s.pid, s.stream_type)));
        }
        let mut count = 0;
        let mut ignored = 0u32;
        let mut truncated = false;
        let mut rest = head.streams;
        while let Some(es) = next_es(&mut rest) {
            if !pid_is_assignable(es.pid) {
                note(
                    &mut self.stats,
                    sink,
                    AnomalyKind::PsiMalformed,
                    es.pid,
                    offset,
                    u32::from(h.id),
                );
                continue;
            }
            if count == per {
                ignored += 1;
                continue;
            }
            let info = stream_info(es.stream_type, es.pid, es.info);
            truncated |= info.flags & SF_DESCRIPTORS_TRUNCATED != 0;
            if info.encryption != Encryption::None {
                let detail = u32::from(es.stream_type);
                note(&mut self.stats, sink, AnomalyKind::EncryptedStream, es.pid, offset, detail);
            }
            area[count] = info;
            count += 1;
        }
        p.pcr_pid = head.pcr_pid;
        p.registration = registration_of(head.program_info);
        p.version = h.version;
        p.mapped = true;
        p.stream_count = count as u16;
        if ignored > 0 {
            note(&mut self.stats, sink, AnomalyKind::StreamLimit, pid, offset, ignored);
        }
        if truncated {
            note(
                &mut self.stats,
                sink,
                AnomalyKind::DescriptorTruncated,
                pid,
                offset,
                u32::from(h.id),
            );
        }
        self.follow(start, count, sink);
        self.rebuild(offset, sink);
        if let (Some(program), Some(streams)) =
            (self.programs.get(index), self.streams.get(start..start + count))
        {
            sink(Event::Pmt { program, streams });
        }
    }

    /// A selected stream the new map at `start` moved to another PID is selected there, the one
    /// of its stream type at its place among that type's streams, unless that PID was told. The
    /// unbounded unit in progress on a PID the map left out ends there.
    fn follow<F: FnMut(Event<'_>)>(&mut self, start: usize, count: usize, sink: &mut F) {
        let Some(now) = self.streams.get(start..start + count) else { return };
        for s in &mut self.pes {
            let gone = self.was.iter().any(|w| w.0 == s.pid) && !now.iter().any(|n| n.pid == s.pid);
            if gone && s.state == PesState::Payload && !s.bounded {
                emit_unit(s, &s.buf, 0, &mut self.stats, sink);
                s.state = PesState::Free;
                s.buf.clear();
            }
        }
        for (n, &(pid, kind)) in self.was.iter().enumerate() {
            let on = self.pids.get(usize::from(pid)).is_some_and(|e| e.flags & EF_ON != 0);
            if !on || now.iter().any(|s| s.pid == pid) {
                continue;
            }
            let nth = self.was[..n].iter().filter(|w| w.1 == kind).count();
            let Some(to) = now.iter().filter(|s| s.stream_type == kind).nth(nth) else { continue };
            if let Some(e) = self.pids.get_mut(usize::from(to.pid))
                && e.flags & (EF_ON | EF_OFF) == 0
            {
                e.flags |= EF_ON;
            }
        }
    }

    /// Slots of PIDs still in use keep their state, the rest are released.
    fn rebuild<F: FnMut(Event<'_>)>(&mut self, offset: u64, sink: &mut F) {
        self.map_dirty = false;
        let select_all = self.cfg.select_all;
        let per = usize::from(self.cfg.limits.max_streams_per_program);
        let reserve = self.cfg.limits.pes_reserve_bytes.max(MIN_PES_BYTES) as usize;
        for &pid in &self.routed {
            if let Some(e) = self.pids.get_mut(usize::from(pid)) {
                e.kind = KIND_NONE;
                e.flags &= !EF_PCR;
            }
        }
        self.routed.clear();
        for s in &mut self.psi_slots {
            s.wanted = false;
        }
        for s in &mut self.pes {
            s.wanted = false;
        }
        if let (Some(e), Some(s)) =
            (self.pids.get_mut(usize::from(PID_PAT)), self.psi_slots.get_mut(0))
        {
            e.kind = KIND_PSI;
            e.slot = 0;
            s.wanted = true;
            self.routed.push(PID_PAT);
        }
        // Free slots of dropped map PIDs first so a full PAT swap finds room.
        for s in self.psi_slots.iter_mut().skip(1) {
            if s.used && !self.programs.iter().any(|p| p.pmt_pid == s.pid) {
                s.used = false;
                s.len = 0;
            }
        }
        // Several programs may share a map PID (2.4.4.8).
        for p in &self.programs {
            let Some(e) = self.pids.get_mut(usize::from(p.pmt_pid)) else { continue };
            if e.kind == KIND_PSI {
                continue;
            }
            let found = self
                .psi_slots
                .iter()
                .position(|s| s.used && s.pid == p.pmt_pid)
                .or_else(|| self.psi_slots.iter().position(|s| !s.used));
            // There is one slot per program plus the PAT, so one is always free.
            let Some(i) = found else { continue };
            let s = &mut self.psi_slots[i];
            if !s.used {
                s.used = true;
                s.pid = p.pmt_pid;
                s.len = 0;
            }
            s.wanted = true;
            e.kind = KIND_PSI;
            e.slot = i as u16;
            debug_assert!(self.routed.len() < self.routed.capacity());
            self.routed.push(p.pmt_pid);
        }
        for p in &self.programs {
            if !p.mapped {
                continue;
            }
            if let Some(e) = self.pids.get_mut(usize::from(p.pcr_pid))
                && p.pcr_pid != PID_NULL
                && e.flags & EF_PCR == 0
            {
                e.flags |= EF_PCR;
                debug_assert!(self.routed.len() < self.routed.capacity());
                self.routed.push(p.pcr_pid);
            }
            let start = usize::from(p.block) * per;
            let Some(streams) =
                self.streams.get(start..start + usize::from(p.stream_count).min(per))
            else {
                continue;
            };
            for info in streams {
                let Some(e) = self.pids.get_mut(usize::from(info.pid)) else { continue };
                if e.kind == KIND_PSI {
                    note(
                        &mut self.stats,
                        sink,
                        AnomalyKind::PidConflict,
                        info.pid,
                        offset,
                        u32::from(p.number),
                    );
                    continue;
                }
                // Nothing downstream can use an encrypted stream unless it asked for it.
                let encrypted = info.encryption != Encryption::None;
                let selected =
                    e.flags & EF_ON != 0 || (select_all && !encrypted && e.flags & EF_OFF == 0);
                // A PID shared by two programs is reassembled once.
                if e.kind == KIND_PES || !selected || !carries_pes(info.stream_type) {
                    continue;
                }
                let found = self
                    .pes
                    .iter()
                    .position(|s| s.state != PesState::Free && s.pid == info.pid)
                    .or_else(|| self.pes.iter().position(|s| s.state == PesState::Free));
                let Some(i) = found else {
                    note(&mut self.stats, sink, AnomalyKind::PesPidLimit, info.pid, offset, 0);
                    continue;
                };
                let s = &mut self.pes[i];
                if s.state == PesState::Free {
                    s.buf.clear();
                    if s.buf.try_reserve_exact(reserve).is_err() {
                        note(&mut self.stats, sink, AnomalyKind::PesAlloc, info.pid, offset, 0);
                        continue;
                    }
                    s.state = PesState::Idle;
                    s.pid = info.pid;
                    s.pending = 0;
                }
                s.wanted = true;
                s.always = if encrypted { UF_ENCRYPTED } else { 0 };
                e.kind = KIND_PES;
                e.slot = i as u16;
                debug_assert!(self.routed.len() < self.routed.capacity());
                self.routed.push(info.pid);
            }
        }
        for s in self.psi_slots.iter_mut().skip(1) {
            if s.used && !s.wanted {
                s.used = false;
                s.len = 0;
            }
        }
        for s in &mut self.pes {
            if s.state != PesState::Free && !s.wanted {
                s.state = PesState::Free;
                s.buf.clear();
                // A freed slot keeps only the warm reserve.
                s.buf.shrink_to(reserve);
            }
        }
    }
}
