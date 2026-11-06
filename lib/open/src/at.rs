// App-side setup before any pipeline, the buffers of one open.
#![allow(clippy::disallowed_methods)]

use std::mem::take;
use std::sync::Arc;

use pf_avi::{AviIndex, Idx1At, Idx1Body};
use pf_bare::{BareHead, BareKind, RawTags};
use pf_mkv::CuesAt;
use pf_mkv::walk::{self, MetaOwned};
use pf_probe::head::{AviHeadAt, MkvHead, MkvHeadAt, Mp4HeadOwned};
use pf_probe::{Kind, PREFIX_LEN, probe};
use profluens_core::fetch::{Fetch, Need, Step};

use crate::{Caps, Desc, Index, Meta, OpenError, Opened, Want, avi, bare, flv, mkv, mp4, ogg};

/// The read in flight, or the inner machine that asks the next one.
enum Stage {
    Start,
    /// The first read into the window.
    Window,
    /// The window grown past `from`, an Ogg comment or the FLV head.
    Grow {
        from: usize,
    },
    /// The file from `tail_at` to its end.
    Tail,
    MkvHead(MkvHeadAt),
    Cues(CuesAt),
    Meta(Box<MetaOwned<Arc<[u8]>>>),
    Mp4Head(Mp4HeadOwned),
    AviHead(AviHeadAt),
    Idx1(Idx1At),
    Bare(Box<pf_bare::OpenAt>),
    Done,
}

/// What a Matroska open carries between its reads.
#[derive(Default)]
struct MkvPart {
    /// The TrackUID of each described track, in their order.
    uids: Vec<u64>,
    /// Where the SeekHead puts the Cues, when they are wanted.
    cues_at: Option<u64>,
    /// The head parsed, the Cues joining it once read.
    parsed: pf_mkv::Parsed,
    /// The Tags and the Attachments by the head, its SeekHead or the head walk past a cut.
    passed: mkv::Passed,
}

/// Opens a file of `len` bytes of any container pf reads, its tracks, the head its demuxer
/// starts from and what [`Want`] asks for. One window read seeds the sniff and every head that
/// fits in it, the inner machines read only past what is held. Matroska Cues and Tags near the
/// end come in one read.
pub struct OpenAt {
    len: u64,
    want: Want,
    caps: Caps,
    stage: Stage,
    got: usize,
    kind: Kind,
    /// The file's first bytes, kept while nothing owns them. Needs inside them or the head are
    /// answered from them.
    window: Vec<u8>,
    /// The file from `tail_at` to its end, Needs inside it are answered from it.
    tail: Vec<u8>,
    tail_at: u64,
    head: Arc<[u8]>,
    desc: Desc,
    duration: u64,
    index: Index,
    meta: Meta,
    mkv: MkvPart,
    ogg: Option<ogg::Scan>,
    /// Where an AVI's `movi` lies, for the reads behind its head.
    movi: Option<pf_avi::MoviLocation>,
}

impl OpenAt {
    pub fn new(len: u64, want: Want, caps: Caps) -> OpenAt {
        OpenAt {
            len,
            want,
            caps,
            stage: Stage::Start,
            got: 0,
            kind: Kind::Mkv,
            window: Vec::new(),
            tail: Vec::new(),
            tail_at: 0,
            head: Arc::from(&[][..]),
            desc: Desc::default(),
            duration: 0,
            index: Index::None,
            meta: Meta::None,
            mkv: MkvPart::default(),
            ogg: None,
            movi: None,
        }
    }

    /// The stage after its read, the next read, None once opened.
    fn step(&mut self) -> Result<Option<Need>, OpenError> {
        loop {
            let next = match &mut self.stage {
                Stage::Start => self.start(),
                Stage::Window => {
                    self.window.truncate(self.got);
                    self.sniff()
                }
                Stage::Grow { from } => {
                    let from = *from;
                    self.window.truncate(from + self.got);
                    self.grown(from)
                }
                Stage::Tail => {
                    self.tail.truncate(self.got);
                    match self.kind {
                        Kind::Ogg => self.ogg_done(),
                        _ => self.mkv_cues(),
                    }
                }
                Stage::MkvHead(m) => match m.poll().map_err(OpenError::MkvHead)? {
                    Step::Need(n) => return Ok(Some(n)),
                    Step::Done(h) => self.mkv_head(h),
                },
                Stage::Cues(m) => match m.poll() {
                    Ok(Step::Need(n)) => return Ok(Some(n)),
                    Ok(Step::Done(ix)) => self.mkv_cued(ix.cues, ix.at),
                    Err(e) => match e {},
                },
                Stage::Meta(m) => match m.poll() {
                    Ok(Step::Need(n)) => return Ok(Some(n)),
                    Ok(Step::Done(meta)) => self.mkv_meta_done(meta),
                    Err(e) => match e {},
                },
                Stage::Mp4Head(m) => match m.poll().map_err(OpenError::Mp4Head)? {
                    Step::Need(n) => return Ok(Some(n)),
                    Step::Done(head) => self.mp4(Arc::from(head)),
                },
                Stage::AviHead(m) => match m.poll().map_err(OpenError::AviHead)? {
                    Step::Need(n) => return Ok(Some(n)),
                    Step::Done(head) => self.avi(Arc::from(head)),
                },
                Stage::Idx1(m) => match m.poll() {
                    Ok(Step::Need(n)) => return Ok(Some(n)),
                    Ok(Step::Done(idx1)) => self.avi_index(idx1),
                    Err(e) => match e {},
                },
                Stage::Bare(m) => match m.poll().map_err(OpenError::Bare)? {
                    Step::Need(n) => return Ok(Some(n)),
                    Step::Done((h, raw)) => self.bare(h, raw),
                },
                Stage::Done => return Ok(None),
            }?;
            if next.is_some() {
                return Ok(next);
            }
        }
    }

    fn done(&mut self) -> Result<Option<Need>, OpenError> {
        self.stage = Stage::Done;
        Ok(None)
    }

    fn start(&mut self) -> Result<Option<Need>, OpenError> {
        let n = usize::try_from(self.len).map_or(self.caps.window, |l| l.min(self.caps.window));
        self.window.clear();
        self.window.resize(n, 0);
        (self.stage, self.got) = (Stage::Window, 0);
        Ok((n > 0).then_some(Need { at: 0, len: n }))
    }

    /// The window grown to `to` bytes, the read of what is past it.
    fn grow(&mut self, to: usize) -> Option<Need> {
        let from = self.window.len();
        self.window.resize(to, 0);
        self.stage = Stage::Grow { from };
        Some(Need { at: from as u64, len: to - from })
    }

    /// The file from `at` to its end into the tail.
    fn read_tail(&mut self, at: u64) -> Option<Need> {
        let n = (self.len - at) as usize;
        (self.tail_at, self.stage) = (at, Stage::Tail);
        self.tail.clear();
        self.tail.resize(n, 0);
        Some(Need { at, len: n })
    }

    /// The bytes held from `at` on, and whether they run to the end of the file. The window
    /// and the head both start the file, the longer of them is looked in.
    fn held_from(&self, at: u64) -> Option<(&[u8], bool)> {
        let front = match self.window.len() >= self.head.len() {
            true => &self.window[..],
            false => &self.head[..],
        };
        let whole = front.len() as u64 == self.len;
        if at < front.len() as u64 || (whole && at == self.len) {
            return Some((&front[at as usize..], whole));
        }
        let from = at.checked_sub(self.tail_at)?;
        (!self.tail.is_empty() && from <= self.tail.len() as u64)
            .then(|| (&self.tail[from as usize..], true))
    }

    fn sniff(&mut self) -> Result<Option<Need>, OpenError> {
        let prefix = &self.window[..self.window.len().min(PREFIX_LEN)];
        self.kind = probe(prefix).map_err(OpenError::Unknown)?;
        let max = usize::try_from(self.caps.max_head).unwrap_or(usize::MAX);
        match self.kind {
            Kind::Mkv => match mkv::head_in(&self.window, self.caps.max_head) {
                Some((n, first_cluster, passed)) => {
                    let head = Arc::from(&self.window[..n]);
                    self.mkv_described(head, first_cluster, passed)
                }
                None => {
                    // kept past the head, which ends before Attachments the meta walk reads
                    let m = MkvHeadAt::with_window(self.caps.max_head, self.window.clone());
                    self.stage = Stage::MkvHead(m);
                    Ok(None)
                }
            },
            Kind::Mp4 => match mp4::head_in(&self.window, self.len) {
                Some(n) => self.mp4(Arc::from(&self.window[..n])),
                None => {
                    self.stage =
                        Stage::Mp4Head(Mp4HeadOwned::new(take(&mut self.window), self.len));
                    Ok(None)
                }
            },
            Kind::Avi => match avi::movi_in(&self.window, max) {
                Some(n) => self.avi(Arc::from(&self.window[..n])),
                None => {
                    self.stage =
                        Stage::AviHead(AviHeadAt::with_window(max, take(&mut self.window)));
                    Ok(None)
                }
            },
            Kind::Ogg => {
                let mut scan = ogg::Scan::new();
                scan.push(&self.window, &mut self.desc, self.want.meta);
                self.ogg = Some(scan);
                self.ogg_more()
            }
            Kind::Flv => self.flv_more(),
            Kind::Flac | Kind::Mp3 | Kind::AdtsAac => {
                let bk = match self.kind {
                    Kind::Flac => BareKind::Flac,
                    Kind::Mp3 => BareKind::Mp3,
                    _ => BareKind::Adts,
                };
                let m = pf_bare::OpenAt::with_head(self.len, bk, take(&mut self.window));
                self.stage = Stage::Bare(Box::new(m));
                Ok(None)
            }
            k => Err(OpenError::Unsupported(k)),
        }
    }

    fn grown(&mut self, from: usize) -> Result<Option<Need>, OpenError> {
        match (self.kind, self.ogg.as_mut()) {
            (Kind::Ogg, Some(scan)) => {
                scan.push(&self.window[from..], &mut self.desc, self.want.meta);
                self.ogg_more()
            }
            (Kind::Avi, _) => {
                self.avi_vol();
                self.avi_tail()
            }
            _ => self.flv_done(),
        }
    }

    fn mkv_head(&mut self, h: MkvHead) -> Result<Option<Need>, OpenError> {
        let passed = mkv::Passed { tags: h.tags, attachments: h.attachments };
        self.mkv_described(Arc::from(h.head), h.first_cluster, passed)
    }

    /// The tracks of `head`, then one read of the Cues and Tags near the end when nothing holds
    /// them.
    fn mkv_described(
        &mut self,
        head: Arc<[u8]>,
        first_cluster: u64,
        walked: mkv::Passed,
    ) -> Result<Option<Need>, OpenError> {
        let mut parsed = pf_mkv::Parsed { first_cluster, ..mkv::parse(&head)? };
        if let Some(ix) = parsed.index.as_mut() {
            ix.tags = ix.tags.or(walked.tags);
            ix.attachments = ix.attachments.or(walked.attachments);
        }
        let (duration, uids) = mkv::describe(&parsed, &mut self.desc)?;
        let (cues, tags, attachments) = mkv::places(&parsed);
        let (len, want) = (self.len, self.want);
        let cues_at = cues.filter(|_| want.index);
        let passed = mkv::Passed { tags, attachments };
        self.mkv = MkvPart { uids, cues_at, parsed, passed };
        (self.duration, self.head) = (duration, head);
        let held = (self.window.len() as u64).max(self.head.len() as u64);
        let meta = [tags, attachments].map(|p| p.filter(|_| want.meta));
        let far = [self.mkv.cues_at, meta[0], meta[1]].into_iter().flatten();
        let far = far.filter(|&p| p >= held && p < len && len - p <= self.caps.max_tail).min();
        match far {
            Some(at) => Ok(self.read_tail(at)),
            None => self.mkv_cues(),
        }
    }

    fn mkv_cues(&mut self) -> Result<Option<Need>, OpenError> {
        let Some(at) = self.mkv.cues_at else { return self.mkv_meta() };
        let max = self.caps.max_cues;
        let parsed = &self.mkv.parsed;
        let held = self.held_from(at).and_then(|(b, eof)| mkv::cues_in(b, eof, max, parsed));
        match held {
            Some(cues) => {
                let at = (!cues.is_empty()).then_some(at);
                self.mkv_cued(cues, at)
            }
            None => {
                self.stage = Stage::Cues(CuesAt::new(&self.head, max));
                Ok(None)
            }
        }
    }

    fn mkv_cued(
        &mut self,
        cues: Vec<pf_mkv::Cue>,
        at: Option<u64>,
    ) -> Result<Option<Need>, OpenError> {
        (self.mkv.parsed.cues, self.mkv.parsed.cued_at) = (Arc::from(cues), at);
        self.mkv_meta()
    }

    fn mkv_meta(&mut self) -> Result<Option<Need>, OpenError> {
        if !self.want.meta {
            return self.mkv_done();
        }
        let (tags, pictures) = (self.caps.max_tags, self.caps.max_picture);
        let m = MetaOwned::new(Arc::clone(&self.head), tags, pictures)
            .with_places(self.mkv.passed.tags, self.mkv.passed.attachments);
        self.stage = Stage::Meta(Box::new(m));
        Ok(None)
    }

    /// The bitrates the statistics tags state for tracks whose head states none.
    fn mkv_meta_done(&mut self, meta: walk::Meta) -> Result<Option<Need>, OpenError> {
        if !meta.tags.is_empty() {
            for (t, &uid) in self.desc.tracks.iter_mut().zip(&self.mkv.uids) {
                if t.bitrate == 0 && uid != 0 {
                    t.bitrate = pf_mkv::tags::bitrate_of(&meta.tags, uid).unwrap_or(0);
                }
            }
        }
        self.meta = Meta::Mkv(meta);
        self.mkv_done()
    }

    fn mkv_done(&mut self) -> Result<Option<Need>, OpenError> {
        self.index = Index::Mkv(Arc::new(take(&mut self.mkv.parsed)));
        self.done()
    }

    fn mp4(&mut self, head: Arc<[u8]>) -> Result<Option<Need>, OpenError> {
        if head.len() as u64 > self.caps.max_head {
            return Err(OpenError::TooLarge);
        }
        (self.duration, self.index) =
            mp4::describe(&head, self.len, self.want.index, &mut self.desc)?;
        if self.want.meta
            && let pf_mp4::MoovExtent::InPrefix { offset, len } =
                pf_mp4::locate_moov(&head, head.len() as u64)
        {
            self.meta = Meta::Moov(offset..offset + len);
        }
        self.head = head;
        self.done()
    }

    fn avi(&mut self, head: Arc<[u8]>) -> Result<Option<Need>, OpenError> {
        let (duration, movi) = avi::describe(&head, &mut self.desc)?;
        (self.duration, self.head, self.movi) = (duration, head, movi);
        // the VOL of an MPEG-4 stream from the bytes held past movi, else one read of them
        if let Some(m) = movi
            && avi::wants_vol(&self.desc)
            && !self.avi_vol()
        {
            let to = m.movi_data_start.saturating_add(avi::VOL_WINDOW).min(self.len);
            if to > self.window.len() as u64 {
                if self.window.len() < self.head.len() {
                    self.window = self.head.to_vec();
                }
                return Ok(self.grow(to as usize));
            }
        }
        self.avi_tail()
    }

    /// The VOL of an MPEG-4 track from the window past `movi`, whether the tracks are settled.
    fn avi_vol(&mut self) -> bool {
        let Some(m) = self.movi else { return true };
        let at = usize::try_from(m.movi_data_start).unwrap_or(usize::MAX);
        match self.window.get(at..) {
            Some(movi) if !movi.is_empty() => avi::vol_from_movi(movi, &mut self.desc),
            _ => false,
        }
    }

    /// The idx1 read when wanted, else the open is done.
    fn avi_tail(&mut self) -> Result<Option<Need>, OpenError> {
        match self.movi.filter(|_| self.want.index) {
            Some(movi) => {
                self.stage = Stage::Idx1(Idx1At::new(movi.movi_end, self.len));
                Ok(None)
            }
            None => self.done(),
        }
    }

    fn avi_index(&mut self, idx1: Option<Idx1Body>) -> Result<Option<Need>, OpenError> {
        let index = AviIndex::build(&self.head, idx1.as_ref(), Some(self.len));
        self.index = index.map_or(Index::None, |i| Index::Avi(Arc::new(i)));
        self.done()
    }

    /// The window grown while the first stream's comment may follow, then the last page's
    /// granule.
    fn ogg_more(&mut self) -> Result<Option<Need>, OpenError> {
        let have = self.window.len() as u64;
        let cap =
            (self.caps.window as u64).saturating_add(self.caps.max_tags.max(self.caps.max_picture));
        let wants = self.ogg.as_ref().is_some_and(ogg::Scan::wants_comment);
        if self.want.meta && wants && have > 0 && have < self.len && have < cap {
            let to = have.saturating_mul(2).min(self.len).min(cap);
            return Ok(self.grow(to as usize));
        }
        if self.len > have {
            let at = self.len.saturating_sub(pf_ogg::OGG_TAIL_LEN as u64).max(have);
            return Ok(self.read_tail(at));
        }
        self.ogg_done()
    }

    fn ogg_done(&mut self) -> Result<Option<Need>, OpenError> {
        let scan = self.ogg.take().ok_or(OpenError::NoStreams)?;
        let tail = match self.tail.is_empty() {
            true => {
                let n = self.window.len().min(pf_ogg::OGG_TAIL_LEN);
                &self.window[self.window.len() - n..]
            }
            false => &self.tail[..],
        };
        let (duration, comment) = scan.finish(tail, self.len, &mut self.desc)?;
        self.duration = duration;
        if let Some(c) = comment.filter(|_| self.want.meta) {
            self.meta = Meta::Ogg(c);
        }
        self.head = Arc::from(take(&mut self.window));
        self.done()
    }

    /// The window grown to the head the FLV demuxer starts from.
    fn flv_more(&mut self) -> Result<Option<Need>, OpenError> {
        let to = self.len.min(pf_flv::HEAD as u64) as usize;
        match self.window.len() < to {
            true => Ok(self.grow(to)),
            false => self.flv_done(),
        }
    }

    fn flv_done(&mut self) -> Result<Option<Need>, OpenError> {
        self.window.truncate(pf_flv::HEAD);
        self.duration = flv::describe(&self.window, &mut self.desc)?;
        self.head = Arc::from(take(&mut self.window));
        self.done()
    }

    fn bare(&mut self, h: BareHead, raw: RawTags) -> Result<Option<Need>, OpenError> {
        bare::describe(&h, &mut self.desc);
        self.duration = h.duration_ns.max(0) as u64;
        if self.want.meta {
            self.meta = Meta::Bare { raw, flac: self.kind == Kind::Flac };
        }
        self.index = Index::Bare(Arc::new(h));
        self.done()
    }

    /// Answers `need` from the window, the head or the tail when they hold it, for the inner
    /// machines.
    fn serve(&mut self, need: Need) -> bool {
        let OpenAt { stage, window, head, tail, tail_at, len, .. } = self;
        let buf = match stage {
            Stage::Cues(m) => m.buf(),
            Stage::Meta(m) => m.buf(),
            Stage::Idx1(m) => m.buf(),
            _ => return false,
        };
        let end = need.at.saturating_add(need.len as u64);
        let held = |at: u64, b: &[u8]| {
            let b_end = at + b.len() as u64;
            let fits = need.at >= at && need.at <= b_end && (end <= b_end || b_end == *len);
            fits.then(|| (need.at - at) as usize..(end.min(b_end) - at) as usize)
        };
        let front = if window.len() >= head.len() { &window[..] } else { &head[..] };
        let (src, range) = match (held(0, front), held(*tail_at, &tail[..])) {
            (Some(r), _) => (front, r),
            (None, Some(r)) if !tail.is_empty() => (&tail[..], r),
            _ => return false,
        };
        let n = range.len();
        buf[..n].copy_from_slice(&src[range]);
        match stage {
            Stage::Cues(m) => m.filled(n),
            Stage::Meta(m) => m.filled(n),
            Stage::Idx1(m) => m.filled(n),
            _ => {}
        }
        true
    }

    fn opened(&mut self) -> Opened {
        let d = take(&mut self.desc);
        Opened {
            kind: self.kind,
            len: self.len,
            duration_ns: self.duration,
            head: std::mem::replace(&mut self.head, Arc::from(&[][..])),
            tracks: d.tracks,
            arena: d.arena,
            index: take(&mut self.index),
            meta: take(&mut self.meta),
            skipped: d.skipped,
        }
    }
}

impl Fetch for OpenAt {
    type Out = Opened;
    type Err = OpenError;

    fn poll(&mut self) -> Result<Step<Opened>, OpenError> {
        loop {
            match self.step() {
                Err(e) => {
                    self.stage = Stage::Done;
                    return Err(e);
                }
                Ok(Some(need)) if self.serve(need) => {}
                Ok(Some(need)) => return Ok(Step::Need(need)),
                Ok(None) => return Ok(Step::Done(self.opened())),
            }
        }
    }

    fn buf(&mut self) -> &mut [u8] {
        match &mut self.stage {
            Stage::Window => &mut self.window,
            Stage::Grow { from } => {
                let from = *from;
                &mut self.window[from..]
            }
            Stage::Tail => &mut self.tail,
            Stage::MkvHead(m) => m.buf(),
            Stage::Cues(m) => m.buf(),
            Stage::Meta(m) => m.buf(),
            Stage::Mp4Head(m) => m.buf(),
            Stage::AviHead(m) => m.buf(),
            Stage::Idx1(m) => m.buf(),
            Stage::Bare(m) => m.buf(),
            Stage::Start | Stage::Done => &mut [],
        }
    }

    fn filled(&mut self, n: usize) {
        match &mut self.stage {
            Stage::MkvHead(m) => m.filled(n),
            Stage::Cues(m) => m.filled(n),
            Stage::Meta(m) => m.filled(n),
            Stage::Mp4Head(m) => m.filled(n),
            Stage::AviHead(m) => m.filled(n),
            Stage::Idx1(m) => m.filled(n),
            Stage::Bare(m) => m.filled(n),
            _ => self.got = n.min(self.buf().len()),
        }
    }
}
