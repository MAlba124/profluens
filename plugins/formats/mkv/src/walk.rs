use std::convert::Infallible;
use std::fmt;

use profluens_core::fetch::{Fetch, Need, Step};

use crate::ebml::{self, ReadError, id};
use crate::extras::Attachment;
use crate::reader::{ELEMENT_HEAD, Frame, MatroskaReader, read_uint};
use crate::tags::{index_segment, is_cover_name};

/// Element heads a [`TrackEnd`] reads at most, a few seconds of a busy file take thousands.
pub const MAX_HEADS: usize = 1 << 18;
/// A Chapters element past this is left out, room for [`crate::extras::MAX_CHAPTERS`] atoms.
pub const MAX_CHAPTERS_LEN: u64 = 1 << 20;
/// An attachment's name, media type or description past this is read as empty.
const MAX_LABEL: u64 = 1 << 10;
/// Bytes of a Cluster read at a time while looking for its keyframe.
const KEY_CHUNK: usize = 256 << 10;
/// A Block's track number and stamp fit in this.
const BLOCK_HEAD: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalkError {
    /// No element header where one must be, or a size or value that cannot be.
    Malformed { at: u64 },
    /// The demuxer refused the Cluster bytes read from `at`.
    Damaged { at: u64, cause: ReadError },
    /// More than [`MAX_HEADS`] element heads read, the last at `at`.
    TooMany { at: u64 },
}

impl fmt::Display for WalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalkError::Malformed { at } => write!(f, "no element header at byte {at}"),
            WalkError::Damaged { at, cause } => write!(f, "the Cluster read at byte {at}, {cause}"),
            WalkError::TooMany { at } => {
                write!(f, "more than {MAX_HEADS} element heads read, the last at byte {at}")
            }
        }
    }
}

impl std::error::Error for WalkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WalkError::Damaged { cause, .. } => Some(cause),
            _ => None,
        }
    }
}

/// An element's head at a file offset, where its data starts and ends.
#[derive(Clone, Copy, Debug)]
struct Head {
    id: [u8; 8],
    id_len: usize,
    data: u64,
    end: Option<u64>,
}

impl Head {
    fn is(&self, want: &[u8]) -> bool {
        self.id.get(..self.id_len) == Some(want)
    }
}

/// The head in `b`, read at `at`. None at the end of the file or for bytes that hold none.
fn head_in(b: &[u8], at: u64) -> Result<Option<Head>, WalkError> {
    let Ok(h) = ebml::read_element_header(b, 0) else { return Ok(None) };
    let mut id = [0u8; 8];
    let id_len = h.id.len().min(8);
    id[..id_len].copy_from_slice(&h.id[..id_len]);
    let data = at.checked_add(h.data_start as u64).ok_or(WalkError::Malformed { at })?;
    let end = match h.size {
        Some(s) => Some(data.checked_add(s).ok_or(WalkError::Malformed { at })?),
        None => None,
    };
    Ok(Some(Head { id, id_len, data, end }))
}

/// A string element's value as read.
// Cold path, once per attachment field.
#[allow(clippy::disallowed_methods)]
fn label(b: &[u8]) -> String {
    String::from_utf8_lossy(ebml::read_string(b)).into_owned()
}

/// A file's Tags, Chapters and image attachments as [`MetaAt`] reads them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    /// The Tags element with its head, empty without one or past the cap.
    pub tags: Vec<u8>,
    /// Images of the Attachments, covers first then in file order.
    pub pictures: Vec<Attachment>,
    /// What stopped the walk, the first error. What was read before it is kept.
    pub error: Option<WalkError>,
    /// The Chapters element with its head, empty without one, past [`MAX_CHAPTERS_LEN`] or
    /// malformed. A bad Chapters is no error, the Tags are read all the same.
    pub chapters: Vec<u8>,
}

/// An AttachedFile's fields by where [`MetaAt`] keeps their spans, in the order it reads them.
const MIME: usize = 0;
const UID: usize = 1;
const NAME: usize = 2;
const DESC: usize = 3;
const DATA: usize = 4;

/// The read in flight of [`MetaAt`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetaState {
    Start,
    /// The Chapters element's head at `at`.
    ChapHead {
        at: u64,
    },
    /// The Chapters whole.
    Chapters,
    /// The Tags element's head at `at`.
    TagsHead {
        at: u64,
    },
    /// The Tags whole.
    Tags,
    /// The Attachments element's head at `at`.
    AttHead {
        at: u64,
    },
    /// An AttachedFile's head at `at`.
    File {
        at: u64,
    },
    /// A child head of the AttachedFile at `at`.
    Field {
        at: u64,
    },
    /// An image's field, [`MIME`] to [`DATA`].
    Pic {
        field: usize,
    },
}

/// The Chapters, Tags and image attachments the SeekHead of a head names. Tags in the head need
/// no read, past it one, and Tags past `max_tags` bytes are left out, Chapters likewise past
/// [`MAX_CHAPTERS_LEN`]. The Attachments are walked by their elements' heads, an attachment's
/// media type is read first and an image's other fields and data of `max_picture` bytes or less
/// after it, fonts stay unread. Whatever the head holds is
/// taken from it unread. An error in the Tags still walks the Attachments, one in the
/// Attachments keeps the images before it.
#[derive(Debug)]
pub struct MetaAt<'h> {
    head: &'h [u8],
    walk: MetaWalk,
}

impl<'h> MetaAt<'h> {
    pub fn new(head: &'h [u8], max_tags: u64, max_picture: u64) -> Self {
        MetaAt { head, walk: MetaWalk::new(head, max_tags, max_picture) }
    }
}

impl Fetch for MetaAt<'_> {
    type Out = Meta;
    type Err = Infallible;

    fn poll(&mut self) -> Result<Step<Meta>, Infallible> {
        Ok(self.walk.poll(self.head))
    }

    fn buf(&mut self) -> &mut [u8] {
        self.walk.buf()
    }

    fn filled(&mut self, n: usize) {
        self.walk.filled(n);
    }
}

/// [`MetaAt`] owning its head, a `Vec<u8>` or an `Arc<[u8]>` shared with the demuxer, to sit
/// inside a machine that owns its bytes.
#[derive(Debug)]
pub struct MetaOwned<H> {
    head: H,
    walk: MetaWalk,
}

impl<H: AsRef<[u8]>> MetaOwned<H> {
    pub fn new(head: H, max_tags: u64, max_picture: u64) -> Self {
        let walk = MetaWalk::new(head.as_ref(), max_tags, max_picture);
        MetaOwned { head, walk }
    }

    pub fn into_head(self) -> H {
        self.head
    }

    /// The Tags and the Attachments at `tags` and `attachments` where the head names neither,
    /// a head cut before them whose SeekHead leaves them out.
    pub fn with_places(mut self, tags: Option<u64>, attachments: Option<u64>) -> Self {
        if let Some(p) = &mut self.walk.index {
            (p.tags, p.attachments) = (p.tags.or(tags), p.attachments.or(attachments));
        }
        self
    }

    /// The Chapters at `chapters` where the head names none, as [`MetaOwned::with_places`].
    pub fn with_chapters(mut self, chapters: Option<u64>) -> Self {
        if let Some(p) = &mut self.walk.index {
            p.chapters = p.chapters.or(chapters);
        }
        self
    }
}

impl<H: AsRef<[u8]>> Fetch for MetaOwned<H> {
    type Out = Meta;
    type Err = Infallible;

    fn poll(&mut self) -> Result<Step<Meta>, Infallible> {
        Ok(self.walk.poll(self.head.as_ref()))
    }

    fn buf(&mut self) -> &mut [u8] {
        self.walk.buf()
    }

    fn filled(&mut self, n: usize) {
        self.walk.filled(n);
    }
}

/// Where the walk finds each master, absolute.
#[derive(Clone, Copy, Debug)]
struct Places {
    tags: Option<u64>,
    attachments: Option<u64>,
    chapters: Option<u64>,
}

/// The walk of [`MetaAt`] and [`MetaOwned`], the head handed in at each poll.
#[derive(Debug)]
struct MetaWalk {
    /// The SeekHead's masters, None for a head with no Segment.
    index: Option<Places>,
    max_tags: u64,
    max_picture: u64,
    state: MetaState,
    hd: [u8; ELEMENT_HEAD],
    /// The Chapters, the Tags, a label or an image's data, read whole.
    whole: Vec<u8>,
    /// Bytes of the read in flight, in `hd` or `whole`.
    len: usize,
    got: usize,
    meta: Meta,
    att_end: u64,
    file_end: u64,
    covers: usize,
    /// The spans of the AttachedFile's fields by [`MIME`] to [`DATA`].
    spans: [Option<(u64, u64)>; 5],
    pic: Attachment,
}

/// Keeps `b` as the image's `field`, a label or the UID.
fn keep(pic: &mut Attachment, field: usize, b: &[u8]) {
    match field {
        MIME => pic.media_type = label(b),
        UID => pic.uid = read_uint(b),
        NAME => pic.name = label(b),
        _ => pic.description = label(b),
    }
}

impl MetaWalk {
    // Empty until the Tags or a field is read whole, cold, once per item.
    #[allow(clippy::disallowed_methods)]
    fn new(head: &[u8], max_tags: u64, max_picture: u64) -> Self {
        MetaWalk {
            index: index_segment(head).map(|ix| Places {
                tags: ix.tags,
                attachments: ix.attachments,
                chapters: ix.chapters,
            }),
            max_tags,
            max_picture,
            state: MetaState::Start,
            hd: [0; ELEMENT_HEAD],
            whole: Vec::new(),
            len: 0,
            got: 0,
            meta: Meta::default(),
            att_end: 0,
            file_end: 0,
            covers: 0,
            spans: [None; 5],
            pic: Attachment::default(),
        }
    }

    /// `len` bytes at `at` into `hd`, read in `state`.
    fn small_at(&mut self, state: MetaState, at: u64, len: usize) -> Option<Need> {
        (self.state, self.len) = (state, len);
        Some(Need { at, len })
    }

    /// `len` bytes at `at` into `whole`, read in `state`.
    fn whole_at(&mut self, state: MetaState, at: u64, len: usize) -> Option<Need> {
        self.whole.clear();
        self.whole.resize(len, 0);
        (self.state, self.len) = (state, len);
        Some(Need { at, len })
    }

    /// The element whose head was read at `at`.
    fn element(&self, at: u64) -> Result<Head, WalkError> {
        head_in(&self.hd[..self.got], at)?.ok_or(WalkError::Malformed { at })
    }

    /// The Tags once the Chapters are done, else the Attachments.
    fn tags(&mut self) -> Option<Need> {
        match self.index?.tags {
            Some(at) => self.small_at(MetaState::TagsHead { at }, at, ELEMENT_HEAD),
            None => self.attachments(),
        }
    }

    /// The Attachments once the Tags are done, None when the walk is.
    fn attachments(&mut self) -> Option<Need> {
        let at = self.index?.attachments?;
        self.small_at(MetaState::AttHead { at }, at, ELEMENT_HEAD)
    }

    /// The AttachedFile at `at`, None past the Attachments.
    fn file(&mut self, at: u64) -> Option<Need> {
        if at >= self.att_end {
            return None;
        }
        self.small_at(MetaState::File { at }, at, ELEMENT_HEAD)
    }

    /// The AttachedFile's child at `at`, its fields once past them.
    fn field(&mut self, at: u64) -> Result<Option<Need>, WalkError> {
        if at < self.file_end {
            return Ok(self.small_at(MetaState::Field { at }, at, ELEMENT_HEAD));
        }
        self.pic_from(MIME)
    }

    /// The image's fields from `field` on. Once the media type is read, an AttachedFile that is
    /// no image of `max_picture` bytes or less is left for the next.
    fn pic_from(&mut self, field: usize) -> Result<Option<Need>, WalkError> {
        for f in field..=DATA {
            if f == UID {
                let is_image = self.pic.media_type.get(..6);
                let is_image = is_image.is_some_and(|p| p.eq_ignore_ascii_case("image/"));
                let max = self.max_picture;
                let fits = |&(f, t): &(u64, u64)| is_image && t > f && t - f <= max;
                let Some((from, to)) = self.spans[DATA].filter(fits) else {
                    return Ok(self.file(self.file_end));
                };
                usize::try_from(to - from).map_err(|_| WalkError::Malformed { at: from })?;
            }
            let span = match f {
                UID => self.spans[UID].filter(|&(u, e)| e - u <= 8),
                DATA => self.spans[DATA],
                _ => self.spans[f].filter(|&(from, to)| to - from <= MAX_LABEL),
            };
            let Some((from, to)) = span.filter(|&(from, to)| to > from) else {
                keep(&mut self.pic, f, &[]);
                continue;
            };
            let (state, len) = (MetaState::Pic { field: f }, (to - from) as usize);
            return Ok(match f {
                UID => self.small_at(state, from, len),
                _ => self.whole_at(state, from, len),
            });
        }
        let a = std::mem::take(&mut self.pic);
        if is_cover_name(&a.name) {
            self.meta.pictures.insert(self.covers, a);
            self.covers += 1;
        } else {
            self.meta.pictures.push(a);
        }
        Ok(self.file(self.file_end))
    }

    /// What the read in flight gives, and the next read. None when the walk is done.
    // Cold path, once per item.
    #[allow(clippy::disallowed_methods)]
    fn step(&mut self, head: &[u8]) -> Result<Option<Need>, WalkError> {
        match self.state {
            MetaState::Start => {
                (self.meta, self.covers) = (Meta::default(), 0);
                match self.index.and_then(|p| p.chapters) {
                    Some(at) => Ok(self.small_at(MetaState::ChapHead { at }, at, ELEMENT_HEAD)),
                    None => Ok(self.tags()),
                }
            }
            MetaState::ChapHead { at } => {
                let Ok(el) = self.element(at) else { return Ok(self.tags()) };
                let len = el.end.map(|end| end - at);
                let len = len.filter(|&len| len <= MAX_CHAPTERS_LEN && el.is(id::CHAPTERS));
                let Some(len) = len.and_then(|len| usize::try_from(len).ok()) else {
                    return Ok(self.tags());
                };
                let from = usize::try_from(at).ok();
                match from.and_then(|from| head.get(from..from.checked_add(len)?)) {
                    Some(b) => {
                        self.meta.chapters = b.to_vec();
                        Ok(self.tags())
                    }
                    None => Ok(self.whole_at(MetaState::Chapters, at, len)),
                }
            }
            MetaState::Chapters => {
                self.whole.truncate(self.got);
                self.meta.chapters = std::mem::take(&mut self.whole);
                Ok(self.tags())
            }
            MetaState::TagsHead { at } => {
                let el = self.element(at)?;
                let len = el.end.map(|end| end - at);
                let len = len.filter(|&len| len <= self.max_tags && el.is(id::TAGS));
                let Some(len) = len.and_then(|len| usize::try_from(len).ok()) else {
                    return Ok(self.attachments());
                };
                let from = usize::try_from(at).ok();
                match from.and_then(|from| head.get(from..from.checked_add(len)?)) {
                    Some(b) => {
                        self.meta.tags = b.to_vec();
                        Ok(self.attachments())
                    }
                    None => Ok(self.whole_at(MetaState::Tags, at, len)),
                }
            }
            MetaState::Tags => {
                self.whole.truncate(self.got);
                self.meta.tags = std::mem::take(&mut self.whole);
                Ok(self.attachments())
            }
            MetaState::AttHead { at } => {
                let top = self.element(at)?;
                let Some(end) = top.end.filter(|_| top.is(id::ATTACHMENTS)) else {
                    return Ok(None);
                };
                self.att_end = end;
                Ok(self.file(top.data))
            }
            MetaState::File { at } => {
                let file = self.element(at)?;
                let Some(fe) = file.end else { return Ok(None) };
                self.file_end = fe;
                if !file.is(id::ATTACHED_FILE) {
                    return Ok(self.file(fe));
                }
                (self.spans, self.pic) = ([None; 5], Attachment::default());
                self.field(file.data)
            }
            MetaState::Field { at } => {
                let child = self.element(at)?;
                let Some(ge) = child.end else { return self.pic_from(MIME) };
                let ids = [id::FILE_MEDIA_TYPE, id::FILE_UID, id::FILE_NAME, id::FILE_DESCRIPTION];
                let slot = ids.iter().position(|want| child.is(want));
                if let Some(slot) = slot.or(child.is(id::FILE_DATA).then_some(DATA)) {
                    self.spans[slot] = Some((child.data, ge));
                }
                self.field(ge)
            }
            MetaState::Pic { field: DATA } => {
                self.whole.truncate(self.got);
                self.pic.data = std::mem::take(&mut self.whole);
                self.pic_from(DATA + 1)
            }
            MetaState::Pic { field } => {
                let b = match field {
                    UID => &self.hd[..self.got],
                    _ => &self.whole[..self.got],
                };
                keep(&mut self.pic, field, b);
                self.pic_from(field + 1)
            }
        }
    }
}

impl MetaWalk {
    fn poll(&mut self, head: &[u8]) -> Step<Meta> {
        loop {
            let tags = matches!(self.state, MetaState::TagsHead { .. } | MetaState::Tags);
            let next = match self.step(head) {
                Ok(next) => next,
                Err(e) if tags => {
                    // an error in the Tags still walks the Attachments
                    self.meta.error = Some(e);
                    self.attachments()
                }
                Err(e) => {
                    self.meta.error.get_or_insert(e);
                    None
                }
            };
            match next {
                Some(need) if self.held(head, need) => {}
                Some(need) => return Step::Need(need),
                None => {
                    self.state = MetaState::Start;
                    return Step::Done(std::mem::take(&mut self.meta));
                }
            }
        }
    }

    /// Answers `need` from the head when it holds it. An element head cut at the head's end is
    /// taken when what is there parses.
    fn held(&mut self, head: &[u8], need: Need) -> bool {
        let from = usize::try_from(need.at).ok();
        let Some(have) = from.and_then(|at| head.get(at..)).filter(|h| !h.is_empty()) else {
            return false;
        };
        let n = have.len().min(need.len);
        let heads = matches!(
            self.state,
            MetaState::TagsHead { .. }
                | MetaState::ChapHead { .. }
                | MetaState::AttHead { .. }
                | MetaState::File { .. }
                | MetaState::Field { .. }
        );
        if n < need.len && !(heads && matches!(head_in(&have[..n], need.at), Ok(Some(_)))) {
            return false;
        }
        self.buf()[..n].copy_from_slice(&have[..n]);
        self.filled(n);
        true
    }

    fn buf(&mut self) -> &mut [u8] {
        match self.state {
            MetaState::Chapters
            | MetaState::Tags
            | MetaState::Pic { field: MIME | NAME | DESC | DATA } => &mut self.whole,
            _ => &mut self.hd[..self.len.min(ELEMENT_HEAD)],
        }
    }

    fn filled(&mut self, n: usize) {
        self.got = n.min(self.buf().len());
    }
}

/// The first keyframe of a track in a cued Cluster, through a reader past the head. A lookup
/// starts with [`KeyframeAt::find`], the reader and the chunk buffer are kept for the next.
pub struct KeyframeAt {
    r: MatroskaReader,
    /// Cluster bytes read at a time, reused across lookups.
    chunk: Vec<u8>,
    number: u64,
    /// The chunk in flight starts here.
    at: u64,
    total: usize,
    limit: usize,
    asked: bool,
    got: usize,
}

impl KeyframeAt {
    /// Looks up keyframes through `r`, a reader the head was pushed into.
    // Empty until the first lookup, then one chunk for all of them.
    #[allow(clippy::disallowed_methods)]
    pub fn new(r: MatroskaReader) -> Self {
        KeyframeAt {
            r,
            chunk: Vec::new(),
            number: 0,
            at: 0,
            total: 0,
            limit: 0,
            asked: false,
            got: 0,
        }
    }

    /// Starts the lookup of track `number`'s first keyframe in the Cluster at byte `cluster`.
    /// Its data as the block holds it, NAL units length prefixed. None when the first `limit`
    /// bytes from there hold none. Polled before a lookup or past its answer, it answers None.
    pub fn find(&mut self, number: u64, cluster: u64, limit: usize) -> &mut Self {
        self.r.resync_streaming();
        self.r.restart_at(Some(cluster));
        (self.number, self.at, self.total, self.limit, self.asked) =
            (number, cluster, 0, limit, false);
        self
    }

    /// The reader, to recycle a frame's data into.
    pub fn reader(&mut self) -> &mut MatroskaReader {
        &mut self.r
    }

    pub fn into_reader(self) -> MatroskaReader {
        self.r
    }

    /// The lookup answered, polls past it answer None.
    fn end(&mut self) {
        (self.total, self.limit) = (0, 0);
    }
}

impl Fetch for KeyframeAt {
    type Out = Option<Frame>;
    type Err = WalkError;

    fn poll(&mut self) -> Result<Step<Option<Frame>>, WalkError> {
        if std::mem::take(&mut self.asked) {
            let (at, n) = (self.at, self.got);
            if n == 0 {
                self.end();
                return Ok(Step::Done(None));
            }
            if let Err(cause) = self.r.push(&self.chunk[..n]) {
                self.end();
                return Err(WalkError::Damaged { at, cause });
            }
            while let Some(f) = self.r.next_frame() {
                if f.track_number == self.number && f.keyframe {
                    self.end();
                    return Ok(Step::Done(Some(f)));
                }
                self.r.recycle(f.data);
            }
            self.at += n as u64;
            self.total += n;
        }
        if self.total >= self.limit {
            self.end();
            return Ok(Step::Done(None));
        }
        let len = KEY_CHUNK.min(self.limit - self.total);
        self.chunk.resize(len, 0);
        self.asked = true;
        Ok(Step::Need(Need { at: self.at, len }))
    }

    fn buf(&mut self) -> &mut [u8] {
        &mut self.chunk
    }

    fn filled(&mut self, n: usize) {
        self.got = n.min(self.chunk.len());
    }
}

/// The read in flight of [`TrackEnd`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EndState {
    Start,
    /// A top level element's head at `at`.
    Top,
    /// A Cluster child's head at `c`.
    Child,
    /// A Timestamp's value.
    Stamp,
    /// A SimpleBlock's track and stamp.
    Simple,
    /// A BlockGroup child's head at `g`.
    Group,
    /// A Block's track and stamp.
    Block,
    /// A BlockDuration's value.
    Duration,
}

/// Where track `number`'s last frame ends, ns, in the Clusters from the one at `cluster` to byte
/// `to` (the end of the file at most), the last block's stamp plus its BlockDuration or
/// `frame_ns`. `scale` is the TimestampScale. Only element heads and block stamps are read, into
/// one small buffer. None for no block of the track or a Cluster of unknown size. A head cut
/// short ends the walk with the end so far.
#[derive(Debug)]
pub struct TrackEnd {
    number: u64,
    cluster: u64,
    to: u64,
    scale: i64,
    frame: i64,
    state: EndState,
    b: [u8; ELEMENT_HEAD],
    /// The read in flight, where it starts and how long it is.
    val_at: u64,
    len: usize,
    got: usize,
    end: Option<i64>,
    heads: usize,
    /// The top level element at `at`, its end `next`.
    at: u64,
    next: u64,
    /// The Cluster's Timestamp, its child at `c` ending at `e`.
    ts: i64,
    c: u64,
    e: u64,
    /// A BlockGroup's child at `g` ending at `ge`, its Block's stamp and duration.
    g: u64,
    ge: u64,
    rel: Option<i64>,
    dur: Option<i64>,
}

fn signed(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

impl TrackEnd {
    pub fn new(number: u64, cluster: u64, to: u64, scale: u64, frame_ns: u64) -> Self {
        TrackEnd {
            number,
            cluster,
            to,
            scale: signed(scale.max(1)),
            frame: signed(frame_ns),
            state: EndState::Start,
            b: [0; ELEMENT_HEAD],
            val_at: 0,
            len: 0,
            got: 0,
            end: None,
            heads: 0,
            at: cluster,
            next: 0,
            ts: 0,
            c: 0,
            e: 0,
            g: 0,
            ge: 0,
            rel: None,
            dur: None,
        }
    }

    fn stamp(&self, rel: i64, dur: i64) -> i64 {
        self.ts.saturating_add(rel).saturating_mul(self.scale).saturating_add(dur)
    }

    fn ask(&mut self, state: EndState, at: u64, len: usize) -> Step<Option<i64>> {
        (self.state, self.val_at, self.len) = (state, at, len);
        Step::Need(Need { at, len })
    }

    /// One more element head read, refused past [`MAX_HEADS`].
    fn count(&mut self, at: u64) -> Result<(), WalkError> {
        self.heads += 1;
        if self.heads > MAX_HEADS { Err(WalkError::TooMany { at }) } else { Ok(()) }
    }

    /// The head read, None at the end of the file or for bytes that hold none.
    fn head(&self) -> Result<Option<Head>, WalkError> {
        head_in(&self.b[..self.got], self.val_at)
    }

    /// An unsigned value of `len` bytes at `at` read in `state`, None for an empty one, 0.
    fn uint(
        &mut self,
        state: EndState,
        at: u64,
        len: u64,
    ) -> Result<Option<Step<Option<i64>>>, WalkError> {
        match len {
            0 => Ok(None),
            1..=8 => Ok(Some(self.ask(state, at, len as usize))),
            _ => Err(WalkError::Malformed { at }),
        }
    }

    /// The stamp of the Block read relative to its Cluster, None for another track's.
    fn block_rel(&self) -> Result<Option<i64>, WalkError> {
        let (b, at) = (&self.b[..self.got], self.val_at);
        let Ok((track, n, _)) = ebml::read_vint(b, 0) else {
            return Err(WalkError::Malformed { at });
        };
        if track != self.number {
            return Ok(None);
        }
        let rel = b.get(n..n + 2).ok_or(WalkError::Malformed { at })?;
        Ok(Some(i64::from(i16::from_be_bytes([rel[0], rel[1]]))))
    }

    /// The top level element at `at`, the end once past `to`.
    fn top(&mut self) -> Step<Option<i64>> {
        match self.at < self.to {
            true => self.ask(EndState::Top, self.at, ELEMENT_HEAD),
            false => Step::Done(self.end),
        }
    }

    /// The Cluster's child at `c`, the next top level element once past it.
    fn child(&mut self) -> Result<Step<Option<i64>>, WalkError> {
        if self.c >= self.next {
            self.at = self.next;
            return Ok(self.top());
        }
        self.count(self.c)?;
        Ok(self.ask(EndState::Child, self.c, ELEMENT_HEAD))
    }

    /// The BlockGroup's child at `g`, its block's end and the next Cluster child once past it.
    fn group(&mut self) -> Result<Step<Option<i64>>, WalkError> {
        if self.g >= self.e {
            if let Some(rel) = self.rel {
                self.end = self.end.max(Some(self.stamp(rel, self.dur.unwrap_or(self.frame))));
            }
            self.c = self.e;
            return self.child();
        }
        self.count(self.g)?;
        Ok(self.ask(EndState::Group, self.g, ELEMENT_HEAD))
    }

    fn step(&mut self) -> Result<Step<Option<i64>>, WalkError> {
        match self.state {
            EndState::Start => {
                (self.end, self.heads, self.at) = (None, 0, self.cluster);
                Ok(self.top())
            }
            EndState::Top => {
                let Some(top) = self.head()? else { return Ok(Step::Done(self.end)) };
                self.count(self.at)?;
                let Some(next) = top.end else { return Ok(Step::Done(None)) };
                self.next = next;
                if !top.is(id::CLUSTER) {
                    self.at = next;
                    return Ok(self.top());
                }
                (self.ts, self.c) = (0, top.data);
                self.child()
            }
            EndState::Child => {
                let Some(el) = self.head()? else { return Ok(Step::Done(self.end)) };
                let Some(e) = el.end else { return Ok(Step::Done(None)) };
                self.e = e;
                if el.is(id::TIMESTAMP) {
                    if let Some(need) = self.uint(EndState::Stamp, el.data, e - el.data)? {
                        return Ok(need);
                    }
                    self.ts = 0;
                } else if el.is(id::SIMPLE_BLOCK) {
                    return Ok(self.ask(EndState::Simple, el.data, BLOCK_HEAD));
                } else if el.is(id::BLOCK_GROUP) {
                    (self.rel, self.dur, self.g) = (None, None, el.data);
                    return self.group();
                }
                self.c = e;
                self.child()
            }
            EndState::Stamp => {
                self.ts = signed(read_uint(&self.b[..self.got]));
                self.c = self.e;
                self.child()
            }
            EndState::Simple => {
                if let Some(rel) = self.block_rel()? {
                    self.end = self.end.max(Some(self.stamp(rel, self.frame)));
                }
                self.c = self.e;
                self.child()
            }
            EndState::Group => {
                let Some(gel) = self.head()? else { return Ok(Step::Done(self.end)) };
                let Some(ge) = gel.end else { return Ok(Step::Done(None)) };
                self.ge = ge;
                if gel.is(id::BLOCK) {
                    return Ok(self.ask(EndState::Block, gel.data, BLOCK_HEAD));
                } else if gel.is(id::BLOCK_DURATION) {
                    if let Some(need) = self.uint(EndState::Duration, gel.data, ge - gel.data)? {
                        return Ok(need);
                    }
                    self.dur = Some(0);
                }
                self.g = ge;
                self.group()
            }
            EndState::Block => {
                self.rel = self.block_rel()?;
                self.g = self.ge;
                self.group()
            }
            EndState::Duration => {
                let d = signed(read_uint(&self.b[..self.got]));
                self.dur = Some(d.saturating_mul(self.scale));
                self.g = self.ge;
                self.group()
            }
        }
    }
}

impl Fetch for TrackEnd {
    type Out = Option<i64>;
    type Err = WalkError;

    fn poll(&mut self) -> Result<Step<Option<i64>>, WalkError> {
        let step = self.step();
        if !matches!(step, Ok(Step::Need(_))) {
            self.state = EndState::Start;
        }
        step
    }

    fn buf(&mut self) -> &mut [u8] {
        &mut self.b[..self.len.min(ELEMENT_HEAD)]
    }

    fn filled(&mut self, n: usize) {
        self.got = n.min(self.len.min(ELEMENT_HEAD));
    }
}

/// [`TrackEnd`] over the spans between the Clusters `starts` names (ascending), the last to the
/// end of the file, walked from the end back. The first span with a frame of the track holds its
/// last, the spans before it stay unread.
#[derive(Debug)]
pub struct LastEnd<'s> {
    starts: &'s [u64],
    number: u64,
    scale: u64,
    frame_ns: u64,
    /// Spans not walked yet, the last of them in `walk` once it is started.
    left: usize,
    walk: Option<TrackEnd>,
}

impl<'s> LastEnd<'s> {
    pub fn new(number: u64, starts: &'s [u64], scale: u64, frame_ns: u64) -> Self {
        LastEnd { starts, number, scale, frame_ns, left: starts.len(), walk: None }
    }

    fn reset(&mut self) {
        (self.left, self.walk) = (self.starts.len(), None);
    }
}

impl Fetch for LastEnd<'_> {
    type Out = Option<i64>;
    type Err = WalkError;

    fn poll(&mut self) -> Result<Step<Option<i64>>, WalkError> {
        loop {
            if self.walk.is_none() {
                let Some(i) = self.left.checked_sub(1) else {
                    self.reset();
                    return Ok(Step::Done(None));
                };
                self.left = i;
                let to = self.starts.get(i + 1).copied().unwrap_or(u64::MAX);
                let from = self.starts.get(i).copied().unwrap_or(u64::MAX);
                self.walk = Some(TrackEnd::new(self.number, from, to, self.scale, self.frame_ns));
            }
            let Some(walk) = &mut self.walk else { continue };
            match walk.poll() {
                Ok(Step::Need(need)) => return Ok(Step::Need(need)),
                Ok(Step::Done(None)) => self.walk = None,
                Ok(Step::Done(Some(end))) => {
                    self.reset();
                    return Ok(Step::Done(Some(end)));
                }
                Err(e) => {
                    self.reset();
                    return Err(e);
                }
            }
        }
    }

    fn buf(&mut self) -> &mut [u8] {
        match &mut self.walk {
            Some(w) => w.buf(),
            None => &mut [],
        }
    }

    fn filled(&mut self, n: usize) {
        if let Some(w) = &mut self.walk {
            w.filled(n);
        }
    }
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::reader::CuesAt;
    use crate::writer::{MatroskaWriter, TrackConfig};
    use profluens_core::fetch::{drive_slice, drive_slice_log};

    fn el(out: &mut Vec<u8>, id: &[u8], body: &[u8]) {
        ebml::write_id(out, id);
        ebml::write_size(out, body.len() as u64);
        out.extend_from_slice(body);
    }

    fn master(id: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        el(&mut out, id, &children.concat());
        out
    }

    fn leaf(id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        el(&mut out, id, body);
        out
    }

    /// The first read `m` hands the caller.
    fn first_need<F: Fetch>(m: &mut F) -> Need {
        match m.poll() {
            Ok(Step::Need(need)) => need,
            _ => unreachable!("no read first"),
        }
    }

    fn block(track: u8, rel: i16) -> Vec<u8> {
        let mut b = vec![0x80 | track];
        b.extend_from_slice(&rel.to_be_bytes());
        b.extend_from_slice(&[0x80, 1, 2, 3]);
        b
    }

    fn cluster(ts: u8, body: &[u8]) -> Vec<u8> {
        let mut c = leaf(id::TIMESTAMP, &[ts]);
        c.extend_from_slice(body);
        leaf(id::CLUSTER, &c)
    }

    fn end_of(file: &[u8], track: u64) -> Option<i64> {
        let mut log = Vec::new();
        let mut m = TrackEnd::new(track, 0, u64::MAX, 1_000_000, 16_000_000);
        let end = drive_slice_log(&mut m, file, &mut log).unwrap();
        assert!(log.iter().all(|n| n.len <= ELEMENT_HEAD), "heads and stamps alone, {log:?}");
        assert_eq!(drive_slice(&mut m, file).unwrap(), end, "a walk that is done starts over");
        end
    }

    #[test]
    fn a_track_ends_at_its_last_block_whatever_follows() {
        let mut blocks = leaf(id::SIMPLE_BLOCK, &block(1, 0));
        el(&mut blocks, id::SIMPLE_BLOCK, &block(2, 30));
        el(&mut blocks, id::SIMPLE_BLOCK, &block(1, 16));
        let mut file = cluster(100, &blocks);
        file.extend(cluster(200, &leaf(id::SIMPLE_BLOCK, &block(2, 50))));
        el(&mut file, id::TAGS, &[0; 40]);
        assert_eq!(end_of(&file, 1), Some(132_000_000), "116 ms and a frame");
        assert_eq!(end_of(&file, 2), Some(266_000_000), "past the other track's end");
        assert_eq!(end_of(&file, 3), None, "no block of it");
    }

    /// The video's last frame in the last span that has one, the spans of later Clusters
    /// without video walked first and the ones before it never read.
    #[test]
    fn the_last_span_with_the_track_holds_its_end() {
        let mut av = leaf(id::SIMPLE_BLOCK, &block(1, 0));
        el(&mut av, id::SIMPLE_BLOCK, &block(2, 5));
        let mut file = cluster(100, &av);
        let second = file.len() as u64;
        file.extend(cluster(150, &leaf(id::SIMPLE_BLOCK, &block(1, 20))));
        let third = file.len() as u64;
        file.extend(cluster(250, &leaf(id::SIMPLE_BLOCK, &block(3, 0))));
        let mut log = Vec::new();
        let starts = [0, second, third];
        let mut m = LastEnd::new(1, &starts, 1_000_000, 16_000_000);
        let end = drive_slice_log(&mut m, &file, &mut log).unwrap();
        assert_eq!(end, Some(186_000_000), "170 ms and a frame, in the second Cluster");
        let lowest = log.iter().map(|n| n.at).min();
        assert_eq!(lowest, Some(second), "the first Cluster is not read");
        assert_eq!(log.first().map(|n| n.at), Some(third), "the last span walked first");
        assert_eq!(drive_slice(&mut m, &file).unwrap(), end, "a walk that is done starts over");
        let mut none = LastEnd::new(4, &starts, 1_000_000, 16_000_000);
        assert_eq!(drive_slice(&mut none, &file).unwrap(), None);
        assert_eq!(drive_slice(&mut LastEnd::new(1, &[], 1_000_000, 16_000_000), &file), Ok(None));
    }

    #[test]
    fn a_block_group_ends_by_its_duration() {
        let mut g = leaf(id::BLOCK, &block(1, -10));
        el(&mut g, id::BLOCK_DURATION, &[40]);
        assert_eq!(end_of(&cluster(100, &leaf(id::BLOCK_GROUP, &g)), 1), Some(130_000_000));
        // an empty BlockDuration is 0 and an empty Timestamp the Cluster's start, both unread
        let mut g = leaf(id::BLOCK, &block(1, 4));
        el(&mut g, id::BLOCK_DURATION, &[]);
        let c = [leaf(id::TIMESTAMP, &[]), leaf(id::BLOCK_GROUP, &g)].concat();
        assert_eq!(end_of(&leaf(id::CLUSTER, &c), 1), Some(4_000_000));
    }

    #[test]
    fn an_unknown_size_gives_no_end_and_a_cut_tail_the_end_so_far() {
        let blocks = leaf(id::SIMPLE_BLOCK, &block(1, 0));
        let mut file = cluster(10, &blocks);
        file.extend_from_slice(&[0x1F, 0x43, 0xB6, 0x75, 0x01, 0xFF, 0xFF]);
        assert_eq!(end_of(&file, 1), Some(26_000_000), "a head cut short ends the walk");
        let mut open = vec![0x1F, 0x43, 0xB6, 0x75, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        open.extend_from_slice(&blocks);
        assert_eq!(end_of(&open, 1), None);
    }

    #[test]
    fn huge_stamps_saturate_and_a_wide_timestamp_is_malformed() {
        let ts = leaf(id::TIMESTAMP, &u64::MAX.to_be_bytes());
        let file = leaf(id::CLUSTER, &[ts, leaf(id::SIMPLE_BLOCK, &block(1, 1))].concat());
        assert_eq!(end_of(&file, 1), Some(i64::MAX));
        let wide = leaf(id::CLUSTER, &leaf(id::TIMESTAMP, &[0; 9]));
        let got = drive_slice(&mut TrackEnd::new(1, 0, u64::MAX, 1, 0), &wide);
        assert!(matches!(got, Err(WalkError::Malformed { .. })), "{got:?}");
    }

    #[test]
    fn a_walk_past_the_head_cap_is_refused() {
        let voids = [0xEC, 0x80].repeat(MAX_HEADS + 1);
        let file = leaf(id::CLUSTER, &voids);
        let mut log = Vec::new();
        let got = drive_slice_log(&mut TrackEnd::new(1, 0, u64::MAX, 1, 0), &file, &mut log);
        assert!(matches!(got, Err(WalkError::TooMany { .. })), "{got:?}");
        assert_eq!(log.len(), MAX_HEADS, "the head past the cap is never read");
    }

    #[test]
    fn a_read_is_the_callers() {
        let need = first_need(&mut TrackEnd::new(1, 70, u64::MAX, 1, 0));
        assert_eq!(need, Need { at: 70, len: ELEMENT_HEAD });
        assert_eq!(
            drive_slice(&mut TrackEnd::new(1, 0, 0, 1, 0), &[]),
            Ok(None),
            "nothing to walk"
        );
    }

    fn cued_video() -> Vec<u8> {
        let tracks = vec![TrackConfig::vp8(1, 320, 240), TrackConfig::vp8(2, 160, 120)];
        let mut w = MatroskaWriter::new(tracks);
        w.enable_cues();
        let mut out = Vec::new();
        w.write_header(&mut out).unwrap();
        for i in 0..12u64 {
            let ns = i * 500_000_000;
            // every video keyframe opens a Cluster, the other track's behind it
            w.write_frame(&mut out, 1, ns, &[0xA0, i as u8, 7, 7], i % 4 == 0).unwrap();
            w.write_frame(&mut out, 2, ns, &[0xB0, i as u8], true).unwrap();
        }
        w.finalize(&mut out);
        out
    }

    #[test]
    fn a_keyframe_is_read_from_its_cluster_alone() {
        let file = cued_video();
        let cl = file.windows(4).position(|w| w == id::CLUSTER).unwrap();
        let head = &file[..cl];
        let ix = drive_slice(&mut CuesAt::new(head, 1 << 20), &file).unwrap();
        let cues: Vec<_> = ix.cues.iter().filter(|c| c.track == 1).collect();
        assert_eq!(cues.len(), 3, "a cue per video keyframe");
        let mut r = MatroskaReader::new();
        r.push(head).unwrap();
        assert!(r.tracks_ready());
        let mut k = KeyframeAt::new(r);
        assert_eq!(drive_slice(&mut k, &file), Ok(None), "no lookup, no read");
        let mut log = Vec::new();
        let mut chunk = None;
        // backwards, each lookup stands alone
        for c in cues.iter().rev() {
            log.clear();
            let f = drive_slice_log(k.find(1, c.byte, 1 << 20), &file, &mut log);
            let f = f.unwrap().expect("a keyframe");
            assert_eq!((f.track_number, f.pts_ns), (1, c.time_ns));
            assert_eq!(f.data, [0xA0, (c.time_ns / 500_000_000) as u8, 7, 7]);
            assert!(log.iter().all(|n| n.at >= c.byte), "nothing before it");
            k.reader().recycle(f.data);
            let f =
                drive_slice(k.find(2, c.byte, 1 << 20), &file).unwrap().expect("past the video's");
            assert_eq!((f.pts_ns, f.data[0]), (c.time_ns, 0xB0));
            let at = *chunk.get_or_insert(k.chunk.as_ptr());
            assert_eq!(at, k.chunk.as_ptr(), "one chunk buffer for every lookup");
        }
        assert_eq!(drive_slice(k.find(1, cues[1].byte, 4), &file), Ok(None), "short limit");
        assert_eq!(drive_slice(k.find(3, cues[1].byte, 1 << 20), &file), Ok(None));
        let need = first_need(k.find(1, cues[1].byte, 1 << 20));
        assert_eq!(need, Need { at: cues[1].byte, len: KEY_CHUNK }, "the read is the caller's");
        let r = k.into_reader();
        assert!(r.tracks_ready(), "the reader comes back");
    }

    fn attached(name: &str, mime: &str, data: &[u8]) -> Vec<u8> {
        let mut uid = Vec::new();
        ebml::write_uint(&mut uid, id::FILE_UID, 0x1234);
        master(
            id::ATTACHED_FILE,
            &[
                leaf(id::FILE_NAME, name.as_bytes()),
                leaf(id::FILE_MEDIA_TYPE, mime.as_bytes()),
                leaf(id::FILE_DESCRIPTION, b"desc"),
                leaf(id::FILE_DATA, data),
                uid,
            ],
        )
    }

    /// A file with Tags and Attachments behind its Cluster, the SeekHead naming both, the head
    /// and where each lies.
    fn meta_file(attachments: &[u8]) -> (Vec<u8>, usize, u64, u64) {
        let tags = master(id::TAGS, &[master(id::TAG, &[leaf(id::SIMPLE_TAG, b"x")])]);
        let ebml_head = master(id::EBML, &[leaf(id::DOC_TYPE, b"matroska")]);
        // 8 octet positions keep the SeekHead's size whatever they hold
        let seek = |pos: u64, target: &[u8]| {
            master(
                id::SEEK,
                &[leaf(id::SEEK_ID, target), leaf(id::SEEK_POSITION, &pos.to_be_bytes())],
            )
        };
        let seek_head =
            |t: u64, a: u64| master(id::SEEK_HEAD, &[seek(t, id::TAGS), seek(a, id::ATTACHMENTS)]);
        let cluster = cluster(0, &leaf(id::SIMPLE_BLOCK, &block(1, 0)));
        let tags_pos = (seek_head(0, 0).len() + cluster.len()) as u64;
        let att_pos = tags_pos + tags.len() as u64;
        let body = [seek_head(tags_pos, att_pos), cluster, tags, attachments.to_vec()].concat();
        let mut file = ebml_head;
        ebml::write_id(&mut file, id::SEGMENT);
        ebml::write_size(&mut file, body.len() as u64);
        let data = file.len() as u64;
        let head = file.len() + seek_head(0, 0).len();
        file.extend_from_slice(&body);
        (file, head, data + tags_pos, data + att_pos)
    }

    #[test]
    fn the_tags_and_images_are_read_by_the_seek_head_fonts_left_unread() {
        let font = vec![9u8; 50_000];
        let att = master(
            id::ATTACHMENTS,
            &[
                attached("art.png", "image/png", &[1, 2]),
                attached("font.ttf", "font/ttf", &font),
                attached("big.jpg", "image/jpeg", &[3; 300]),
                attached("Cover.JPG", "IMAGE/jpeg", &[4, 5, 6]),
            ],
        );
        let (file, head, tags_at, _) = meta_file(&att);
        let mut log = Vec::new();
        let mut meta = MetaAt::new(&file[..head], 1 << 20, 100);
        let m = drive_slice_log(&mut meta, &file, &mut log).unwrap();
        assert!(m.error.is_none(), "{:?}", m.error);
        let tags_end = tags_at as usize + m.tags.len();
        assert_eq!(m.tags, file[tags_at as usize..tags_end]);
        let got: Vec<_> = m.pictures.iter().map(|a| (a.name.as_str(), a.data.as_slice())).collect();
        assert_eq!(got, [("Cover.JPG", &[4, 5, 6][..]), ("art.png", &[1, 2][..])], "covers first");
        let a = &m.pictures[1];
        assert_eq!(
            (a.uid, a.media_type.as_str(), a.description.as_str()),
            (0x1234, "image/png", "desc")
        );
        assert!(log.iter().all(|n| n.len < 300), "no font or oversized data read");
        assert_eq!(drive_slice(&mut meta, &file).unwrap(), m, "a walk that is done starts over");
        let none = drive_slice(&mut MetaAt::new(&file[..head], 4, 100), &file).unwrap();
        assert!(none.tags.is_empty() && none.error.is_none(), "Tags past the cap left out");
        log.clear();
        let plain =
            drive_slice_log(&mut MetaAt::new(b"not matroska", 1 << 20, 100), &file, &mut log);
        let plain = plain.unwrap();
        assert!(plain.tags.is_empty() && plain.pictures.is_empty() && plain.error.is_none());
        assert!(log.is_empty());
        // Tags the head holds need no read
        let held = drive_slice_log(&mut MetaAt::new(&file, 1 << 20, 100), &file, &mut log);
        assert_eq!(held.unwrap().tags, m.tags);
        assert!(log.iter().all(|n| n.len <= ELEMENT_HEAD || n.at > tags_end as u64), "{log:?}");
    }

    #[test]
    fn what_the_head_holds_is_taken_unread() {
        let att = master(
            id::ATTACHMENTS,
            &[
                attached("cover.png", "image/png", &[1, 2]),
                attached("font.ttf", "font/ttf", &[9; 500]),
                attached("art.jpg", "image/jpeg", &[3, 4, 5]),
            ],
        );
        let (file, head, _, att_at) = meta_file(&att);
        let mut log = Vec::new();
        let want = drive_slice_log(&mut MetaAt::new(&file[..head], 1 << 20, 100), &file, &mut log);
        let want = want.unwrap();
        assert_eq!(want.pictures.len(), 2);
        // Tags 2, Attachments 1, a file's head 1 and its 5 fields' 5, its media type 1, an
        // image's other 4 fields 4
        assert_eq!(log.len(), 32, "{log:?}");
        for cut in [file.len(), att_at as usize + 40, att_at as usize + 3, att_at as usize] {
            log.clear();
            let got =
                drive_slice_log(&mut MetaAt::new(&file[..cut], 1 << 20, 100), &file, &mut log);
            assert_eq!(got.unwrap(), want, "a head of {cut}");
            let behind = |n: &Need| n.at + n.len as u64 > cut as u64;
            assert!(log.iter().all(behind), "{cut}, {log:?}");
        }
        assert!(log.len() < 32, "{log:?}");
        log.clear();
        let mut m = MetaOwned::new(file.clone(), 1 << 20, 100);
        assert_eq!(drive_slice_log(&mut m, &file, &mut log).unwrap(), want);
        assert!(log.is_empty(), "a whole file as the head reads nothing, {log:?}");
        assert_eq!(m.into_head(), file);
        let shared: std::sync::Arc<[u8]> = file[..head].into();
        let mut m = MetaOwned::new(std::sync::Arc::clone(&shared), 1 << 20, 100);
        assert_eq!(drive_slice(&mut m, &file).unwrap(), want);
        assert_eq!(drive_slice(&mut m, &file).unwrap(), want, "a walk that is done starts over");
    }

    #[test]
    fn a_malformed_attachment_keeps_what_came_before_it() {
        let mut body = attached("cover.png", "image/png", &[1]);
        body.extend_from_slice(&[0x00; 16]);
        body.extend(attached("later.png", "image/png", &[2]));
        let (file, head, tags_at, att_at) = meta_file(&leaf(id::ATTACHMENTS, &body));
        let m = drive_slice(&mut MetaAt::new(&file[..head], 1 << 20, 100), &file).unwrap();
        let bad = att_at + 5 + attached("cover.png", "image/png", &[1]).len() as u64;
        assert!(matches!(m.error, Some(WalkError::Malformed { at }) if at == bad), "{:?}", m.error);
        assert!(!m.tags.is_empty(), "the Tags read before it");
        assert_eq!(m.pictures.len(), 1, "the image before it");
        let need = first_need(&mut MetaAt::new(&file[..head], 1 << 20, 100));
        assert_eq!(need, Need { at: tags_at, len: ELEMENT_HEAD }, "the read is the caller's");
    }

    /// Chapters and Tags behind the Cluster, the SeekHead naming both, and where each lies.
    fn chapters_file(chapters: &[u8]) -> (Vec<u8>, usize, u64) {
        let tags = master(id::TAGS, &[master(id::TAG, &[leaf(id::SIMPLE_TAG, b"x")])]);
        let seek = |pos: u64, target: &[u8]| {
            master(
                id::SEEK,
                &[leaf(id::SEEK_ID, target), leaf(id::SEEK_POSITION, &pos.to_be_bytes())],
            )
        };
        let seek_head =
            |c: u64, t: u64| master(id::SEEK_HEAD, &[seek(c, id::CHAPTERS), seek(t, id::TAGS)]);
        let cluster = cluster(0, &leaf(id::SIMPLE_BLOCK, &block(1, 0)));
        let chap_pos = (seek_head(0, 0).len() + cluster.len()) as u64;
        let tags_pos = chap_pos + chapters.len() as u64;
        let body = [seek_head(chap_pos, tags_pos), cluster, chapters.to_vec(), tags].concat();
        let mut file = master(id::EBML, &[leaf(id::DOC_TYPE, b"matroska")]);
        ebml::write_id(&mut file, id::SEGMENT);
        ebml::write_size(&mut file, body.len() as u64);
        let data = file.len() as u64;
        let head = file.len() + seek_head(0, 0).len();
        file.extend_from_slice(&body);
        (file, head, data + chap_pos)
    }

    #[test]
    fn the_chapters_are_read_by_the_seek_head_before_the_tags() {
        let atom = master(id::CHAPTER_ATOM, &[leaf(id::CHAPTER_TIME_START, &[0])]);
        let chapters = master(id::CHAPTERS, &[master(id::EDITION_ENTRY, &[atom])]);
        let (file, head, at) = chapters_file(&chapters);
        let mut log = Vec::new();
        let m = drive_slice_log(&mut MetaAt::new(&file[..head], 1 << 20, 100), &file, &mut log);
        let m = m.unwrap();
        assert_eq!((m.chapters.as_slice(), m.error), (&chapters[..], None));
        assert!(!m.tags.is_empty(), "the Tags behind them");
        assert_eq!(log[0], Need { at, len: ELEMENT_HEAD }, "{log:?}");
        log.clear();
        let held = drive_slice_log(&mut MetaAt::new(&file, 1 << 20, 100), &file, &mut log);
        assert_eq!(held.unwrap().chapters, chapters, "held in the head, read from it");
        assert!(log.is_empty(), "{log:?}");
        // past the cap, a wrong element and a broken head leave them out, never an error
        let big = master(id::CHAPTERS, &[leaf(id::VOID, &vec![0; MAX_CHAPTERS_LEN as usize])]);
        let (file, head, _) = chapters_file(&big);
        let m = drive_slice(&mut MetaAt::new(&file[..head], 1 << 20, 100), &file).unwrap();
        assert!(m.chapters.is_empty() && m.error.is_none() && !m.tags.is_empty());
        for bad in [master(id::TAGS, &[]), vec![0x00; 16]] {
            let (file, head, _) = chapters_file(&bad);
            let m = drive_slice(&mut MetaAt::new(&file[..head], 1 << 20, 100), &file).unwrap();
            assert!(m.chapters.is_empty() && m.error.is_none(), "{:?}", m.error);
        }
        // a head cut before Chapters its SeekHead leaves out is told where they lie
        let (mut file, head, at) = chapters_file(&chapters);
        let seek_id = file.windows(4).position(|w| w == id::CHAPTERS).unwrap();
        file[seek_id..seek_id + 4].copy_from_slice(id::CLUSTER);
        let blind = MetaOwned::new(file[..head].to_vec(), 1 << 20, 100);
        assert!(drive_slice(&mut { blind }, &file).unwrap().chapters.is_empty());
        let told = MetaOwned::new(file[..head].to_vec(), 1 << 20, 100).with_chapters(Some(at));
        assert_eq!(drive_slice(&mut { told }, &file).unwrap().chapters, chapters);
    }

    #[test]
    fn an_error_in_the_tags_still_walks_the_attachments() {
        let att = master(id::ATTACHMENTS, &[attached("art.png", "image/png", &[1, 2])]);
        let (mut file, head, tags_at, _) = meta_file(&att);
        // a Tags head that holds no element
        file[tags_at as usize] = 0x00;
        let m = drive_slice(&mut MetaAt::new(&file[..head], 1 << 20, 100), &file).unwrap();
        assert!(
            matches!(m.error, Some(WalkError::Malformed { at }) if at == tags_at),
            "{:?}",
            m.error
        );
        assert_eq!(m.pictures.len(), 1, "the image behind them");
    }
}
