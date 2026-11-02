use crate::ctx::SeekTarget;
use crate::feedback::Feedback;
use crate::format::{FixedFormat, Value};
use crate::id::{SinkPad, SrcPad};
use crate::memory::{Memory, Pool};
use crate::time::{Rate, Timestamp};

/// An in-band event, ordered with the buffers around it.
// The format rides inline, boxing it would allocate on the data path.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum Event {
    Segment {
        base: Timestamp,
        rate: f64,
    },
    FormatChange(FixedFormat),
    /// Nothing arrives through running time `until`.
    Gap {
        until: Timestamp,
    },
    Tags(TagList),
    /// A live property changed, delivered at a batch boundary.
    PropChanged {
        name: &'static str,
        value: Value,
    },
    Eos,
    FlushStart,
    FlushStop,
    Paused,
    Resumed,
    /// Presentation skipped forward to `to_time` without a flush.
    SkippedTo {
        to_time: Timestamp,
    },
    /// The app asked for playback rate `rate`.
    RateAsked {
        rate: Rate,
    },
    /// This sink's offset is now `by` nanoseconds.
    Offset {
        by: i64,
    },
    /// A splice linked or unlinked src pad `pad`, optionally restarting its input.
    SrcLinked {
        pad: SrcPad,
        on: bool,
        restart: Option<SeekTarget>,
    },
    /// A splice gave src pad `pad` another link beside the ones it feeds.
    SrcJoined {
        pad: SrcPad,
    },
    /// A splice replaced or removed the upstream of sink pad `pad`.
    SinkLinked {
        pad: SinkPad,
        on: bool,
    },
    /// Overwrite already written output bytes at absolute `offset` with `data`.
    Patch {
        offset: u64,
        data: Memory,
    },
    /// What feeds from src pad `pad` said upstream, the latest of its kind.
    Feedback {
        pad: SrcPad,
        feedback: Feedback,
    },
}

/// How a flush treats an in-band event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventClass {
    State,
    Positional,
}

impl Event {
    pub fn class(&self) -> EventClass {
        match self {
            Event::FormatChange(_) | Event::Tags(_) => EventClass::State,
            Event::Segment { .. } | Event::Gap { .. } | Event::Patch { .. } | Event::Eos => {
                EventClass::Positional
            }
            Event::PropChanged { .. }
            | Event::FlushStart
            | Event::FlushStop
            | Event::Paused
            | Event::Resumed
            | Event::SkippedTo { .. }
            | Event::RateAsked { .. }
            | Event::SrcLinked { .. }
            | Event::SrcJoined { .. }
            | Event::SinkLinked { .. }
            | Event::Feedback { .. }
            | Event::Offset { .. } => EventClass::Positional,
        }
    }
}

/// Stream metadata, Vorbis-comment keys uppercased plus zero-copy pictures.
#[derive(Clone, Default)]
pub struct TagList {
    text: Vec<(Box<str>, Box<str>)>,
    pictures: Vec<Picture>,
}

#[derive(Clone)]
pub struct Picture {
    pub mime: Box<str>,
    pub data: Memory,
}

impl TagList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.pictures.is_empty()
    }

    pub fn len(&self) -> usize {
        self.text.len() + self.pictures.len()
    }

    /// Append a text tag, uppercasing `key`.
    // One-time per stream, built while parsing headers.
    #[allow(clippy::disallowed_methods)]
    pub fn add(&mut self, key: &str, value: &str) -> &mut Self {
        self.text.push((key.to_ascii_uppercase().into(), value.into()));
        self
    }

    #[allow(clippy::disallowed_methods)] // one-time per stream, like `add`
    pub fn add_picture(&mut self, mime: &str, data: Memory) -> &mut Self {
        self.pictures.push(Picture { mime: mime.into(), data });
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.text.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| &**v)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.text.iter().map(|(k, v)| (&**k, &**v))
    }

    pub fn pictures(&self) -> &[Picture] {
        &self.pictures
    }

    pub fn merge(&mut self, other: TagList) {
        self.text.extend(other.text);
        self.pictures.extend(other.pictures);
    }
}

/// Receives tag entries as a parser decodes them. Arguments are borrowed for the call only.
pub trait TagSink {
    fn text(&mut self, key: &str, value: &str);

    fn picture(&mut self, mime: &str, data: &[u8]);

    /// A picture whose container states its type, the ID3v2 APIC numbering FLAC shares, 3 the
    /// front cover. A FLAC type past 255 comes as 255. Forwards to [`TagSink::picture`] unless
    /// the sink keeps the type.
    fn picture_typed(&mut self, kind: u8, mime: &str, data: &[u8]) {
        let _ = kind;
        self.picture(mime, data);
    }
}

/// A [`TagSink`] building an owned [`TagList`], copying pictures into pool memory.
pub struct TagListSink<'p> {
    pool: &'p Pool,
    tags: TagList,
}

impl<'p> TagListSink<'p> {
    pub fn new(pool: &'p Pool) -> Self {
        Self { pool, tags: TagList::new() }
    }

    pub fn finish(self) -> TagList {
        self.tags
    }
}

impl TagSink for TagListSink<'_> {
    fn text(&mut self, key: &str, value: &str) {
        self.tags.add(key, value);
    }

    fn picture(&mut self, mime: &str, data: &[u8]) {
        // A picture past the exact cap is left out, the text tags still go.
        let Some(mut mem) = self.pool.acquire_exact(data.len()) else { return };
        mem.as_mut_full()[..data.len()].copy_from_slice(data);
        mem.set_len(data.len());
        self.tags.add_picture(mime, mem);
    }
}

#[cfg(test)]
mod tag_tests {
    use super::{TagList, TagListSink, TagSink};
    use crate::memory::Pool;

    #[test]
    fn text_tags_add_get_iter() {
        let mut t = TagList::new();
        assert!(t.is_empty());
        t.add("title", "Enough").add("Artist", "Fred again..").add("ARTIST", "Brian Eno");

        assert_eq!(t.get("TITLE"), Some("Enough"));
        assert_eq!(t.get("title"), Some("Enough"));
        assert_eq!(t.get("artist"), Some("Fred again.."));
        assert_eq!(t.get("album"), None);

        let all: Vec<_> = t.iter().collect();
        assert_eq!(
            all,
            vec![("TITLE", "Enough"), ("ARTIST", "Fred again.."), ("ARTIST", "Brian Eno")]
        );
        assert_eq!(t.len(), 3);
        assert!(!t.is_empty());
    }

    #[test]
    fn merge_appends() {
        let mut a = TagList::new();
        a.add("title", "A");
        let mut b = TagList::new();
        b.add("genre", "Ambient");
        a.merge(b);
        assert_eq!(a.get("TITLE"), Some("A"));
        assert_eq!(a.get("GENRE"), Some("Ambient"));
    }

    #[test]
    fn tag_list_sink_materialises_text_and_pictures() {
        let pool = Pool::bounded(64, 4);
        let art = [0xFFu8, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];

        let mut sink = TagListSink::new(&pool);
        sink.text("TITLE", "Enough");
        sink.text("artist", "Fred again..");
        sink.picture("image/jpeg", &art);
        let tags = sink.finish();

        assert_eq!(tags.get("TITLE"), Some("Enough"));
        assert_eq!(tags.get("ARTIST"), Some("Fred again.."));
        assert_eq!(tags.pictures().len(), 1);

        let pic = &tags.pictures()[0];
        assert_eq!(&*pic.mime, "image/jpeg");
        assert_eq!(pic.data.len(), art.len());
        assert_eq!(pic.data.data(), &art);
    }

    #[test]
    fn a_typed_picture_reaches_a_sink_that_drops_the_type() {
        let pool = Pool::bounded(64, 4);
        let mut sink = TagListSink::new(&pool);
        sink.picture_typed(3, "image/png", b"\x89PNG");
        let tags = sink.finish();
        assert_eq!(tags.pictures().len(), 1);
        assert_eq!(&*tags.pictures()[0].mime, "image/png");
        assert_eq!(tags.pictures()[0].data.data(), b"\x89PNG");
    }
}
