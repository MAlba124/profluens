//! AMF0 script data (Annex E.4.4, AMF0 2.x), the `onMetaData` header. Nothing is copied, the
//! `keyframes` arrays are kept as places in the body and read with [`numbers`].

use std::fmt;

/// Nesting past this is refused, so junk cannot run the stack out.
pub const MAX_DEPTH: u8 = 16;

const NUMBER: u8 = 0x00;
const STRING: u8 = 0x02;
const OBJECT: u8 = 0x03;
const ECMA: u8 = 0x08;
const OBJECT_END: u8 = 0x09;
const STRICT: u8 = 0x0A;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AmfError {
    Truncated,
    TooDeep,
    Marker(u8),
    NotMetaData,
}

impl fmt::Display for AmfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AmfError::Truncated => f.write_str("AMF0 data runs past its tag"),
            AmfError::TooDeep => write!(f, "AMF0 nested past {MAX_DEPTH} levels"),
            AmfError::Marker(m) => write!(f, "AMF0 type marker {m:#04x} is not readable"),
            AmfError::NotMetaData => f.write_str("the script tag is not onMetaData"),
        }
    }
}

impl std::error::Error for AmfError {}

/// A strict array, the body offset of its first value and the count it states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Array {
    pub at: usize,
    pub count: u32,
}

/// The `onMetaData` fields the demuxer reads. Numbers are as stated, seconds and pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Meta {
    pub duration: Option<f64>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub video_codec: Option<f64>,
    pub audio_codec: Option<f64>,
    pub filepositions: Option<Array>,
    pub times: Option<Array>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Top,
    Keyframes,
    Skip,
}

struct Cursor<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], AmfError> {
        let end = self.at.checked_add(n).filter(|e| *e <= self.b.len());
        let end = end.ok_or(AmfError::Truncated)?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, AmfError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<usize, AmfError> {
        let b = self.take(2)?;
        Ok(usize::from(u16::from_be_bytes([b[0], b[1]])))
    }

    fn u32(&mut self) -> Result<u32, AmfError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn f64(&mut self) -> Result<f64, AmfError> {
        let b = self.take(8)?;
        Ok(f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }

    fn string(&mut self) -> Result<&'a [u8], AmfError> {
        let n = self.u16()?;
        self.take(n)
    }

    fn left(&self) -> usize {
        self.b.len() - self.at
    }
}

/// The `onMetaData` of a script tag body, behind an `@setDataFrame` as RTMP ingest writes it.
pub fn on_meta_data(body: &[u8]) -> Result<Meta, AmfError> {
    let mut c = Cursor { b: body, at: 0 };
    let mut name = name(&mut c)?;
    if name == b"@setDataFrame" {
        name = self::name(&mut c)?;
    }
    if name != b"onMetaData" {
        return Err(AmfError::NotMetaData);
    }
    let mut meta = Meta::default();
    match c.u8()? {
        ECMA => {
            c.u32()?;
            props(&mut c, 1, Level::Top, &mut meta)?;
        }
        OBJECT => props(&mut c, 1, Level::Top, &mut meta)?,
        m => return Err(AmfError::Marker(m)),
    }
    Ok(meta)
}

fn name<'a>(c: &mut Cursor<'a>) -> Result<&'a [u8], AmfError> {
    match c.u8()? {
        STRING => c.string(),
        m => Err(AmfError::Marker(m)),
    }
}

/// Properties to the end marker. A body that ends between two properties ends them, as an
/// ECMA array written without its marker does.
fn props(c: &mut Cursor<'_>, depth: u8, level: Level, meta: &mut Meta) -> Result<(), AmfError> {
    if depth > MAX_DEPTH {
        return Err(AmfError::TooDeep);
    }
    loop {
        if c.left() == 0 && level == Level::Top {
            return Ok(());
        }
        let key = c.string()?;
        let marker = c.u8()?;
        if key.is_empty() && marker == OBJECT_END {
            return Ok(());
        }
        let slot = match (level, key, marker) {
            (Level::Top, b"duration", NUMBER) => Some(&mut meta.duration),
            (Level::Top, b"width", NUMBER) => Some(&mut meta.width),
            (Level::Top, b"height", NUMBER) => Some(&mut meta.height),
            (Level::Top, b"videocodecid", NUMBER) => Some(&mut meta.video_codec),
            (Level::Top, b"audiocodecid", NUMBER) => Some(&mut meta.audio_codec),
            _ => None,
        };
        if let Some(slot) = slot {
            *slot = Some(c.f64()?);
            continue;
        }
        match (level, key, marker) {
            (Level::Top, b"keyframes", OBJECT) => props(c, depth + 1, Level::Keyframes, meta)?,
            (Level::Keyframes, b"filepositions", STRICT) => {
                meta.filepositions = Some(array(c, depth + 1)?);
            }
            (Level::Keyframes, b"times", STRICT) => meta.times = Some(array(c, depth + 1)?),
            _ => value(c, marker, depth + 1)?,
        }
    }
}

fn array(c: &mut Cursor<'_>, depth: u8) -> Result<Array, AmfError> {
    let count = c.u32()?;
    let at = c.at;
    // Every value is an octet at least.
    if count as usize > c.left() {
        return Err(AmfError::Truncated);
    }
    for _ in 0..count {
        let marker = c.u8()?;
        value(c, marker, depth)?;
    }
    Ok(Array { at, count })
}

/// Steps over one value of type `marker`.
fn value(c: &mut Cursor<'_>, marker: u8, depth: u8) -> Result<(), AmfError> {
    if depth > MAX_DEPTH {
        return Err(AmfError::TooDeep);
    }
    let mut skip = Meta::default();
    match marker {
        NUMBER => c.take(8).map(drop),
        0x01 => c.take(1).map(drop),
        STRING => c.string().map(drop),
        OBJECT => props(c, depth + 1, Level::Skip, &mut skip),
        // Null, undefined, unsupported.
        0x05 | 0x06 | 0x0D => Ok(()),
        0x07 => c.take(2).map(drop),
        ECMA => {
            c.u32()?;
            props(c, depth + 1, Level::Skip, &mut skip)
        }
        STRICT => array(c, depth + 1).map(drop),
        0x0B => c.take(10).map(drop),
        0x0C | 0x0F => {
            let n = c.u32()? as usize;
            c.take(n).map(drop)
        }
        0x10 => {
            c.string()?;
            props(c, depth + 1, Level::Skip, &mut skip)
        }
        m => Err(AmfError::Marker(m)),
    }
}

/// The values of a strict array, `None` for one not a number, to the first unreadable one.
pub fn numbers(body: &[u8], array: Array) -> impl Iterator<Item = Option<f64>> + '_ {
    let mut c = Cursor { b: body, at: array.at };
    let mut left = array.count;
    std::iter::from_fn(move || {
        if left == 0 {
            return None;
        }
        left -= 1;
        let read = match c.u8() {
            Ok(NUMBER) => c.f64().map(Some),
            Ok(m) => value(&mut c, m, 1).map(|()| None),
            Err(e) => Err(e),
        };
        match read {
            Ok(v) => Some(v),
            Err(_) => {
                left = 0;
                None
            }
        }
    })
}

#[cfg(test)]
mod tests {
    // Test bodies.
    #![allow(clippy::disallowed_methods)]

    use super::*;

    fn key(out: &mut Vec<u8>, k: &str) {
        out.extend_from_slice(&(k.len() as u16).to_be_bytes());
        out.extend_from_slice(k.as_bytes());
    }

    fn num(out: &mut Vec<u8>, k: &str, v: f64) {
        key(out, k);
        out.push(NUMBER);
        out.extend_from_slice(&v.to_be_bytes());
    }

    fn strict(out: &mut Vec<u8>, k: &str, vs: &[f64]) {
        key(out, k);
        out.push(STRICT);
        out.extend_from_slice(&(vs.len() as u32).to_be_bytes());
        for v in vs {
            out.push(NUMBER);
            out.extend_from_slice(&v.to_be_bytes());
        }
    }

    fn body(ecma: bool) -> Vec<u8> {
        let mut b = vec![STRING];
        key(&mut b, "onMetaData");
        b.push(if ecma { ECMA } else { OBJECT });
        if ecma {
            b.extend_from_slice(&5u32.to_be_bytes());
        }
        num(&mut b, "duration", 4.25);
        key(&mut b, "encoder");
        b.push(STRING);
        key(&mut b, "Lavf");
        num(&mut b, "width", 128.0);
        key(&mut b, "nested");
        b.push(OBJECT);
        num(&mut b, "duration", 99.0);
        b.extend_from_slice(&[0, 0, OBJECT_END]);
        key(&mut b, "keyframes");
        b.push(OBJECT);
        strict(&mut b, "filepositions", &[701.0, 20760.0]);
        strict(&mut b, "times", &[0.0, 1.0]);
        b.extend_from_slice(&[0, 0, OBJECT_END]);
        num(&mut b, "videocodecid", 7.0);
        b.extend_from_slice(&[0, 0, OBJECT_END]);
        b
    }

    #[test]
    fn the_fields_and_the_keyframe_arrays_are_read() {
        for ecma in [true, false] {
            let b = body(ecma);
            let m = on_meta_data(&b).unwrap();
            assert_eq!((m.duration, m.width, m.height), (Some(4.25), Some(128.0), None));
            assert_eq!((m.video_codec, m.audio_codec), (Some(7.0), None), "a nested one is not");
            let pos: Vec<_> = numbers(&b, m.filepositions.unwrap()).collect();
            let times: Vec<_> = numbers(&b, m.times.unwrap()).collect();
            assert_eq!(pos, [Some(701.0), Some(20760.0)]);
            assert_eq!(times, [Some(0.0), Some(1.0)]);
        }
    }

    #[test]
    fn an_ecma_array_without_its_end_marker_ends_with_the_body() {
        let mut b = body(true);
        b.truncate(b.len() - 3);
        assert_eq!(on_meta_data(&b).unwrap().video_codec, Some(7.0));
        b.truncate(b.len() - 3);
        assert_eq!(on_meta_data(&b), Err(AmfError::Truncated));
    }

    #[test]
    fn junk_is_refused_and_nesting_is_bounded() {
        assert_eq!(on_meta_data(b""), Err(AmfError::Truncated));
        let mut other = vec![STRING];
        key(&mut other, "onCuePoint");
        assert_eq!(on_meta_data(&other), Err(AmfError::NotMetaData));
        let mut deep = vec![STRING];
        key(&mut deep, "onMetaData");
        deep.push(OBJECT);
        for _ in 0..40 {
            key(&mut deep, "a");
            deep.push(OBJECT);
        }
        assert_eq!(on_meta_data(&deep), Err(AmfError::TooDeep));
        let mut huge = vec![STRING];
        key(&mut huge, "onMetaData");
        huge.push(OBJECT);
        key(&mut huge, "x");
        huge.push(STRICT);
        huge.extend_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(on_meta_data(&huge), Err(AmfError::Truncated));
    }

    #[test]
    fn an_array_value_that_is_no_number_reads_as_none() {
        let mut b = vec![NUMBER];
        b.extend_from_slice(&1.5f64.to_be_bytes());
        b.extend_from_slice(&[0x01, 1, 0x05, 0xEE]);
        let got: Vec<_> = numbers(&b, Array { at: 0, count: 5 }).collect();
        assert_eq!(got, [Some(1.5), None, None], "it stops at the marker it cannot read");
    }
}
