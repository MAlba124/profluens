//! What the fake pads offer and what follows from it, modeled on core's negotiation without core.

use core::fmt;

use super::scenario::{MAX_SINKS, MAX_STREAMS, Topology};
use crate::prng::Prng;
use crate::time::NONE;

pub const FAMILIES: [&str; 3] = ["sim/media", "sim/other", "bytes"];
pub const BYTES: u8 = 2;
pub const FIELD: &str = "id";
pub const MAX_IDS: usize = 4;
pub const MAX_OFFERS: usize = 3;
pub const MAX_LINKS: usize = 3;

/// Never zero, and no two streams share one.
pub const fn id_of(stream: u16, n: u8) -> u8 {
    (n % MAX_IDS as u8 + 1) * 10 + stream as u8
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offer {
    pub family: u8,
    pub ids: [u8; MAX_IDS],
    /// How many of `ids` count. Zero takes any id.
    pub id_count: u8,
    /// Nonzero, the ids from `ids[0]` to `ids[1]` this far apart, a stepped range.
    pub step: u8,
}

impl Offer {
    pub const ANY: Offer = Offer { family: 0, ids: [0; MAX_IDS], id_count: 0, step: 0 };
    pub const BYTES: Offer = Offer { family: BYTES, ..Offer::ANY };

    pub fn ids(&self) -> &[u8] {
        &self.ids[..(self.id_count as usize).min(MAX_IDS)]
    }

    pub fn takes(&self, id: u8) -> bool {
        match self.step {
            0 => self.id_count == 0 || self.ids().contains(&id),
            step => {
                self.ids[0] <= id && id <= self.ids[1] && (id - self.ids[0]).is_multiple_of(step)
            }
        }
    }

    fn open(&self) -> bool {
        self.step == 0 && self.id_count == 0
    }

    /// As core's `meet`. Two sets in this side's order, a set and a range in the set's.
    pub fn and(&self, sink: &Offer) -> Option<Offer> {
        if self.family != sink.family {
            return None;
        }
        match (self.open(), sink.open()) {
            (true, _) => return Some(*sink),
            (false, true) => return Some(*self),
            _ => {}
        }
        let mut out = Offer { family: self.family, ..Offer::ANY };
        if self.step > 0 && sink.step > 0 {
            let (lo, hi) = (self.ids[0].max(sink.ids[0]), self.ids[1].min(sink.ids[1]));
            let both = |id: &u8| self.takes(*id) && sink.takes(*id);
            let (first, last) = ((lo..=hi).find(both)?, (lo..=hi).rev().find(both));
            let last = last.filter(|l| *l != first);
            out.ids[0] = first;
            out.id_count = 1;
            if let Some(last) = last {
                let gcd = |mut a: u8, mut b: u8| {
                    while b != 0 {
                        (a, b) = (b, a % b);
                    }
                    a
                };
                (out.ids[1], out.id_count) = (last, 2);
                out.step = self.step / gcd(self.step, sink.step) * sink.step;
            }
            return Some(out);
        }
        let order = if self.step == 0 { self } else { sink };
        for id in order.ids().iter().filter(|id| self.takes(**id) && sink.takes(**id)) {
            out.ids[out.id_count as usize] = *id;
            out.id_count += 1;
        }
        (out.id_count > 0).then_some(out)
    }

    pub fn meet(&self, sink: &Offer) -> Option<Fixed> {
        let met = self.and(sink)?;
        Some(Fixed { family: met.family, id: met.ids().first().copied().unwrap_or(0) })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PadOffers {
    pub list: [Offer; MAX_OFFERS],
    /// With none the pad is not probed and has the static menu [`Offer::ANY`].
    pub count: u8,
}

impl PadOffers {
    pub const OPEN: PadOffers = PadOffers { list: [Offer::ANY; MAX_OFFERS], count: 0 };

    pub const fn probed(&self) -> bool {
        self.count > 0
    }

    pub fn offers(&self) -> &[Offer] {
        &self.list[..(self.count as usize).clamp(1, MAX_OFFERS)]
    }

    pub const fn pinned(fixed: Fixed) -> PadOffers {
        let (ids, id_count) = ([fixed.id; MAX_IDS], (fixed.id != 0) as u8);
        PadOffers {
            list: [Offer { family: fixed.family, ids, id_count, step: 0 }; MAX_OFFERS],
            count: 1,
        }
    }

    pub fn negotiate(&self, sink: &PadOffers) -> Option<Fixed> {
        let theirs = Alts::of(sink.offers());
        let skip = shut(self.offers(), &theirs, &theirs);
        Alts::of(self.offers()).and(&theirs, skip).pick(&theirs, skip)
    }

    pub fn admits(&self, family: u8, id: u8) -> bool {
        self.offers().iter().any(|offer| offer.family == family && offer.takes(id))
    }
}

const MAX_ALTS: usize = MAX_OFFERS * MAX_OFFERS;

#[derive(Clone, Copy, Debug)]
struct Alts {
    list: [Offer; MAX_ALTS],
    n: usize,
    wild: bool,
}

/// Whether a link may not take `bytes` because the sink names a typed family listed before it,
/// as core's `escape`.
fn shut(src: &[Offer], sink: &Alts, takes: &Alts) -> bool {
    let names = |alts: &Alts, family| !alts.wild && alts.get().iter().any(|o| o.family == family);
    let mut shared = false;
    for offer in src {
        if offer.family == BYTES {
            return shared;
        }
        shared = shared || names(sink, offer.family) || names(takes, offer.family);
    }
    false
}

impl Alts {
    const WILD: Alts = Alts { list: [Offer::ANY; MAX_ALTS], n: 0, wild: true };

    fn of(offers: &[Offer]) -> Alts {
        let mut alts =
            Alts { list: [Offer::ANY; MAX_ALTS], n: offers.len().min(MAX_ALTS), wild: false };
        alts.list[..alts.n].copy_from_slice(&offers[..alts.n]);
        alts
    }

    fn pinned(fixed: Fixed) -> Alts {
        Alts::of(PadOffers::pinned(fixed).offers())
    }

    fn get(&self) -> &[Offer] {
        &self.list[..self.n]
    }

    /// As core's `meet_lists`.
    fn and(&self, other: &Alts, skip: bool) -> Alts {
        match (self.wild, other.wild) {
            (true, _) => return *other,
            (false, true) => return *self,
            _ => {}
        }
        let mut out = Alts { n: 0, ..*self };
        for (a, b) in self.get().iter().flat_map(|a| other.get().iter().map(move |b| (a, b))) {
            if let Some(met) = a.and(b).filter(|m| out.n < MAX_ALTS && !(skip && m.family == BYTES))
            {
                out.list[out.n] = met;
                out.n += 1;
            }
        }
        out
    }

    /// As core's fix pass.
    fn pick(&self, peer: &Alts, skip: bool) -> Option<Fixed> {
        let met = match peer.wild {
            true => *self.get().first()?,
            false => self.get().iter().find_map(|a| {
                peer.get().iter().find_map(|b| a.and(b).filter(|m| !(skip && m.family == BYTES)))
            })?,
        };
        Some(Fixed { family: met.family, id: met.ids().first().copied().unwrap_or(0) })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fixed {
    pub family: u8,
    /// Zero is a link that left the id open.
    pub id: u8,
}

impl Fixed {
    pub const OPEN: Fixed = Fixed { family: 0, id: 0 };
}

impl fmt::Display for Fixed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let family = FAMILIES.get(self.family as usize).copied().unwrap_or("?");
        match self.id {
            0 => write!(f, "{family} with the id open"),
            id => write!(f, "{family} id {id}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offers {
    pub srcs: [PadOffers; MAX_STREAMS],
    pub sinks: [PadOffers; MAX_SINKS],
    pub second: [u8; MAX_STREAMS],
}

impl Offers {
    pub const OPEN: Offers = Offers {
        srcs: [PadOffers::OPEN; MAX_STREAMS],
        sinks: [PadOffers::OPEN; MAX_SINKS],
        second: [id_of(0, 1), id_of(1, 1)],
    };

    pub fn is_open(&self) -> bool {
        *self == Offers::OPEN
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub links: [Fixed; MAX_LINKS],
    pub made: u8,
    pub refused: bool,
    src: [Fixed; MAX_STREAMS],
    sink: [Fixed; MAX_SINKS],
    sinks: [PadOffers; MAX_SINKS],
    stream_of: [u8; MAX_SINKS],
    second: [u8; MAX_STREAMS],
    change: [u64; MAX_STREAMS],
}

impl Plan {
    /// Each link solves what it is coupled to, so a sink behind the transform can move what the
    /// source sends.
    pub fn new(topology: Topology, offers: &Offers, change: &[u64; MAX_STREAMS]) -> Plan {
        let mut plan = Plan {
            links: [Fixed::OPEN; MAX_LINKS],
            made: 0,
            refused: false,
            src: [Fixed::OPEN; MAX_STREAMS],
            sink: [Fixed::OPEN; MAX_SINKS],
            sinks: offers.sinks,
            stream_of: [0; MAX_SINKS],
            second: offers.second,
            change: *change,
        };
        let source = Alts::of(offers.srcs[0].offers());
        if topology.has_transform() {
            let Some(fixed) = source.pick(&Alts::WILD, false) else {
                plan.refused = true;
                return plan;
            };
            plan.made(fixed);
            plan.src[0] = fixed;
        }
        let mut carried = Alts::WILD;
        for sink in 0..topology.sinks().min(MAX_SINKS) {
            let stream = (topology.stream_of(sink) as usize).min(MAX_STREAMS - 1);
            plan.stream_of[sink] = stream as u8;
            let theirs = Alts::of(offers.sinks[sink].offers());
            if !topology.through_transform(sink) {
                let skip = shut(offers.srcs[stream].offers(), &theirs, &theirs);
                let ours = Alts::of(offers.srcs[stream].offers()).and(&theirs, skip);
                let Some(fixed) = ours.pick(&theirs, skip) else {
                    plan.refused = true;
                    break;
                };
                (plan.src[stream], plan.sink[sink]) = (fixed, fixed);
                plan.made(fixed);
                continue;
            }
            // What the transform's sink may carry is what every output linked so far takes.
            carried = carried.and(&theirs, false);
            let skip = shut(offers.srcs[0].offers(), &Alts::WILD, &carried);
            let ours = source.and(&carried, skip);
            let Some(fixed) = ours.pick(&carried, skip) else {
                plan.refused = true;
                break;
            };
            plan.src[0] = fixed;
            for out in (0..=sink).filter(|s| topology.through_transform(*s)) {
                let theirs = Alts::of(offers.sinks[out].offers());
                plan.sink[out] =
                    Alts::pinned(fixed).and(&theirs, false).pick(&theirs, false).unwrap_or(fixed);
            }
            plan.made(plan.sink[sink]);
        }
        plan
    }

    fn made(&mut self, fixed: Fixed) {
        if let Some(slot) = self.links.get_mut(self.made as usize) {
            *slot = fixed;
            self.made += 1;
        }
    }

    pub fn family(&self, stream: u16) -> u8 {
        self.src.get(stream as usize).map_or(0, |fixed| fixed.family)
    }

    pub fn first(&self, stream: u16) -> u8 {
        match self.src.get(stream as usize) {
            Some(Fixed { id, .. }) if *id != 0 => *id,
            _ => id_of(stream, 0),
        }
    }

    pub fn format_of(&self, stream: u16, seq: u64) -> u8 {
        let at = stream as usize;
        match self.change.get(at) {
            Some(change) if seq >= *change => self.second[at],
            _ => self.first(stream),
        }
    }

    pub fn told(&self, sink: usize) -> u8 {
        self.sink.get(sink).map_or(0, |fixed| fixed.id)
    }

    pub fn admits(&self, sink: usize, id: u8) -> bool {
        let Some(pad) = self.sinks.get(sink) else {
            return false;
        };
        pad.admits(self.family(self.stream_of[sink] as u16), id)
    }

    pub fn carries(&self, stream: u16, id: u8) -> bool {
        let at = stream as usize;
        let Some(change) = self.change.get(at).copied() else {
            return false;
        };
        (change > 0 && id == self.first(stream)) || (change != NONE && id == self.second[at])
    }

    pub fn turns_away(&self, sink: usize, id: u8) -> bool {
        let Some(stream) = self.stream_of.get(sink) else {
            return false;
        };
        self.carries(*stream as u16, id) && !self.admits(sink, id)
    }

    pub fn turns_some_away(&self, sink: usize) -> bool {
        let Some(stream) = self.stream_of.get(sink).map(|stream| *stream as u16) else {
            return false;
        };
        let second = self.second[stream as usize];
        self.turns_away(sink, self.first(stream)) || self.turns_away(sink, second)
    }
}

fn draw_pad(rng: &mut Prng, stream: u16) -> PadOffers {
    let mut pad = PadOffers::OPEN;
    if rng.chance(15) {
        return pad;
    }
    pad.count = if rng.chance(25) { 2 } else { 1 };
    for offer in pad.list.iter_mut().take(pad.count as usize) {
        offer.family = rng.chance(12) as u8;
        let mut ids: [u8; MAX_IDS] = core::array::from_fn(|n| id_of(stream, n as u8));
        // Durstenfeld, "Algorithm 235: Random permutation", CACM 7(7), 1964.
        for i in (1..MAX_IDS).rev() {
            ids.swap(i, rng.below(i as u64 + 1) as usize);
        }
        offer.ids = ids;
        offer.id_count = match rng.below(8) {
            0 => 0,
            n => 1 + (n as u8 - 1) % MAX_IDS as u8,
        };
    }
    pad
}

fn vary(pad: &mut PadOffers, (step, escape): (Prng, Prng), stream: u16) {
    let (mut step, mut escape) = (step, escape);
    for offer in pad.list.iter_mut().take(pad.count as usize) {
        if offer.id_count == 0 || !step.chance(20) {
            continue;
        }
        let wide = step.chance(50);
        let (by, first) = if wide { (20, step.below(2) as u8) } else { (10, step.below(3) as u8) };
        let last = if wide { first + 2 } else { first + 1 + step.below(3 - first as u64) as u8 };
        (offer.ids[0], offer.ids[1], offer.id_count, offer.step) =
            (id_of(stream, first), id_of(stream, last), 2, by);
    }
    if pad.probed() && (pad.count as usize) < MAX_OFFERS && escape.chance(25) {
        pad.list[pad.count as usize] = Offer::BYTES;
        pad.count += 1;
    }
}

pub fn draw(root: &Prng, topology: Topology) -> Offers {
    let mut offers = Offers::OPEN;
    let mut rng = root.substream_named("offers");
    if !rng.chance(25) {
        return offers;
    }
    let varied = |[step, escape]: [&str; 2], n: u64| {
        (root.substream_named(step).substream(n), root.substream_named(escape).substream(n))
    };
    for stream in 0..topology.streams().min(MAX_STREAMS) {
        let mut pad = root.substream_named("offers.src").substream(stream as u64);
        offers.srcs[stream] = draw_pad(&mut pad, stream as u16);
        vary(
            &mut offers.srcs[stream],
            varied(["offers.src.step", "offers.src.escape"], stream as u64),
            stream as u16,
        );
        offers.second[stream] = id_of(stream as u16, rng.below(MAX_IDS as u64) as u8);
    }
    for sink in 0..topology.sinks().min(MAX_SINKS) {
        let mut pad = root.substream_named("offers.sink").substream(sink as u64);
        offers.sinks[sink] = draw_pad(&mut pad, topology.stream_of(sink));
        vary(
            &mut offers.sinks[sink],
            varied(["offers.sink.step", "offers.sink.escape"], sink as u64),
            topology.stream_of(sink),
        );
    }
    offers
}

fn write_pad(f: &mut fmt::Formatter<'_>, pad: &PadOffers) -> fmt::Result {
    if !pad.probed() {
        return f.write_str("open");
    }
    for (n, offer) in pad.offers().iter().enumerate() {
        let family = FAMILIES.get(offer.family as usize).copied().unwrap_or("?");
        let or = if n > 0 { " or " } else { "" };
        match (offer.ids(), offer.step) {
            _ if offer.family == BYTES => write!(f, "{or}{family}")?,
            ([], _) => write!(f, "{or}{family} any id")?,
            (ids, 0) => write!(f, "{or}{family} ids {ids:?}")?,
            (ids, step) => write!(f, "{or}{family} ids {} to {} step {step}", ids[0], ids[1])?,
        }
    }
    Ok(())
}

pub struct Listed<'a> {
    pub offers: &'a Offers,
    pub streams: usize,
    pub sinks: usize,
}

impl fmt::Display for Listed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for stream in 0..self.streams.min(MAX_STREAMS) {
            write!(f, "  stream {stream} offers ")?;
            write_pad(f, &self.offers.srcs[stream])?;
            writeln!(f, ", changes to id {}", self.offers.second[stream])?;
        }
        for sink in 0..self.sinks.min(MAX_SINKS) {
            write!(f, "  sink {sink} offers ")?;
            write_pad(f, &self.offers.sinks[sink])?;
            writeln!(f)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(family: u8, ids: &[u8]) -> Offer {
        let mut offer = Offer { family, ids: [0; MAX_IDS], id_count: ids.len() as u8, step: 0 };
        offer.ids[..ids.len()].copy_from_slice(ids);
        offer
    }

    fn pad(offers: &[Offer]) -> PadOffers {
        let mut pad = PadOffers { list: [Offer::ANY; MAX_OFFERS], count: offers.len() as u8 };
        pad.list[..offers.len()].copy_from_slice(offers);
        pad
    }

    fn fixed(family: u8, id: u8) -> Option<Fixed> {
        Some(Fixed { family, id })
    }

    #[test]
    fn two_offers_meet_on_the_first_id_of_the_source_the_sink_takes() {
        let src = offer(0, &[30, 10, 20]);
        assert_eq!(src.meet(&offer(0, &[20, 10])), fixed(0, 10));
        assert_eq!(src.meet(&offer(0, &[40])), None, "no id in common");
        assert_eq!(src.meet(&offer(1, &[30])), None, "another family");
        assert_eq!(src.meet(&offer(0, &[])), fixed(0, 30), "any id takes the source's first");
        assert_eq!(offer(0, &[]).meet(&offer(0, &[20, 10])), fixed(0, 20), "and gives the sink's");
        assert_eq!(Offer::ANY.meet(&Offer::ANY), fixed(0, 0), "nobody chose");
    }

    fn stepped(family: u8, min: u8, max: u8, step: u8) -> Offer {
        let mut offer = offer(family, &[min, max]);
        offer.step = step;
        offer
    }

    #[test]
    fn stepped_ranges_meet_on_both_lattices_and_a_set_keeps_its_order() {
        let (all, odd, even) =
            (stepped(0, 10, 40, 10), stepped(0, 10, 30, 20), stepped(0, 20, 40, 20));
        assert!(odd.takes(30) && !odd.takes(20) && !odd.takes(40) && even.takes(40));
        assert_eq!(odd.meet(&even), None, "misaligned, nothing on both lattices");
        assert_eq!(all.and(&even), Some(stepped(0, 20, 40, 20)), "the coarser lattice");
        assert_eq!(all.meet(&even), fixed(0, 20), "a range fixes its lowest");
        assert_eq!(stepped(0, 10, 30, 10).and(&even), Some(offer(0, &[20])), "one value left");
        assert_eq!(
            all.meet(&offer(0, &[30, 20])),
            fixed(0, 30),
            "a range and a set in the set's order"
        );
        assert_eq!(offer(0, &[40, 20]).meet(&odd), None);
        assert_eq!(offer(0, &[40, 20, 10]).meet(&all), fixed(0, 40));
        assert_eq!(Offer::ANY.and(&odd), Some(odd), "any id takes the range");
    }

    #[test]
    fn a_bytes_escape_is_shut_where_the_sink_names_the_typed_family() {
        let src = pad(&[offer(0, &[20]), Offer::BYTES]);
        assert_eq!(src.negotiate(&pad(&[offer(0, &[10]), Offer::BYTES])), None);
        assert_eq!(src.negotiate(&pad(&[offer(1, &[11]), Offer::BYTES])), fixed(BYTES, 0));
        assert_eq!(src.negotiate(&pad(&[Offer::BYTES])), fixed(BYTES, 0));
        assert_eq!(
            pad(&[Offer::BYTES, offer(0, &[20])]).negotiate(&pad(&[offer(0, &[10]), Offer::BYTES])),
            fixed(BYTES, 0)
        );
        let mut offers = Offers::OPEN;
        offers.srcs[0] = src;
        offers.sinks[0] = pad(&[offer(0, &[10]), Offer::BYTES]);
        let none = [NONE; MAX_STREAMS];
        let plan = Plan::new(Topology::Transform, &offers, &none);
        assert_eq!((plan.made, plan.refused), (1, true), "the sink's link is refused");
        offers.sinks[0] = pad(&[offer(1, &[11]), Offer::BYTES]);
        let plan = Plan::new(Topology::Transform, &offers, &none);
        assert_eq!((plan.made, plan.refused, plan.family(0)), (2, false, BYTES));
    }

    #[test]
    fn a_link_fixes_the_first_pair_that_meets_in_the_sources_order() {
        let src = pad(&[offer(1, &[11]), offer(0, &[20, 10])]);
        let sink = pad(&[offer(0, &[10]), offer(1, &[11])]);
        assert_eq!(src.negotiate(&sink), fixed(1, 11), "the source's first offer wins");
        assert_eq!(src.negotiate(&pad(&[offer(0, &[10])])), fixed(0, 10));
        assert_eq!(src.negotiate(&pad(&[offer(0, &[30])])), None);
        assert_eq!(PadOffers::OPEN.negotiate(&PadOffers::OPEN), fixed(0, 0));
        assert_eq!(PadOffers::OPEN.negotiate(&sink), fixed(0, 10));
        assert_eq!(pad(&[offer(1, &[11])]).negotiate(&PadOffers::OPEN), None);
    }

    #[test]
    fn a_transform_carries_what_every_output_takes_and_the_source_sends_it() {
        let src = pad(&[offer(0, &[20, 10]), offer(1, &[11])]);
        let mut offers = Offers::OPEN;
        offers.srcs[0] = src;
        // Linked alone the transport carries the source's first, 20, and the sink's link moves it
        // to 10.
        offers.sinks[0] = pad(&[offer(0, &[10])]);
        let none = [NONE; MAX_STREAMS];
        let through = Plan::new(Topology::Transform, &offers, &none);
        let (first, ten) = (Fixed { family: 0, id: 20 }, Fixed { family: 0, id: 10 });
        assert_eq!(
            (through.made, through.refused, through.links[0], through.links[1]),
            (2, false, first, ten)
        );
        assert_eq!((through.first(0), through.told(0)), (10, 10), "the source is told the sink's");
        let direct = Plan::new(Topology::Line, &offers, &none);
        assert_eq!((direct.made, direct.refused, direct.told(0)), (1, false, 10));
        offers.sinks = [pad(&[offer(0, &[20])]), pad(&[offer(0, &[10, 20])])];
        let fan = Plan::new(Topology::FanOut, &offers, &none);
        assert_eq!((fan.made, fan.refused, fan.told(0), fan.told(1)), (3, false, 20, 20));
        offers.sinks = [pad(&[offer(0, &[20, 10])]), pad(&[offer(0, &[10, 20])])];
        let fan = Plan::new(Topology::FanOut, &offers, &none);
        assert_eq!(
            (fan.made, fan.refused, fan.told(0), fan.told(1), fan.first(0)),
            (3, false, 20, 20, 20)
        );
        offers.sinks = [pad(&[offer(0, &[10])]), pad(&[offer(0, &[20])])];
        let fan = Plan::new(Topology::FanOut, &offers, &none);
        assert_eq!((fan.made, fan.refused), (2, true));
    }

    #[test]
    fn a_plan_names_the_formats_of_a_stream_and_what_a_sink_turns_away() {
        let mut offers = Offers::OPEN;
        offers.srcs[0] = pad(&[offer(0, &[30, 10])]);
        offers.sinks[0] = pad(&[offer(0, &[10, 30])]);
        offers.second[0] = 40;
        let plan = Plan::new(Topology::Line, &offers, &[5, NONE]);
        assert_eq!((plan.first(0), plan.format_of(0, 4), plan.format_of(0, 5)), (30, 30, 40));
        assert!(plan.admits(0, 10) && !plan.admits(0, 40));
        assert!(plan.turns_away(0, 40) && !plan.turns_away(0, 30) && plan.turns_some_away(0));
        assert!(!plan.turns_away(0, 20), "not taken, and never carried");
        assert!(!Plan::new(Topology::Line, &offers, &[NONE; 2]).turns_some_away(0));
        let at_once = Plan::new(Topology::Line, &offers, &[0, NONE]);
        assert!(at_once.carries(0, 40) && !at_once.carries(0, 30));

        offers.srcs[0] = PadOffers::OPEN;
        offers.sinks[0] = pad(&[offer(0, &[30])]);
        let through = Plan::new(Topology::Transform, &offers, &[NONE; 2]);
        assert_eq!((through.refused, through.first(0), through.told(0)), (false, 30, 30));
        assert!(!through.turns_some_away(0));
        let told = Plan::new(Topology::Line, &offers, &[NONE; 2]);
        assert_eq!((told.first(0), told.turns_some_away(0)), (30, false));
    }

    #[test]
    fn an_open_scenario_plays_under_the_formats_it_always_did() {
        for topology in Topology::ALL {
            let plan = Plan::new(topology, &Offers::OPEN, &[7, NONE]);
            assert!(!plan.refused);
            assert_eq!(plan.made as usize, topology.sinks() + topology.has_transform() as usize);
            assert_eq!((plan.format_of(0, 6), plan.format_of(0, 7)), (10, 20), "{topology:?}");
            assert_eq!((plan.format_of(1, 0), plan.format_of(1, 9_999)), (11, 11));
            assert_eq!(plan.told(0), 0);
            assert!(!plan.turns_some_away(0) && !plan.turns_some_away(1));
        }
    }

    #[test]
    fn the_seed_draws_open_pads_choices_and_refusals() {
        let (mut open, mut refused, mut turned, mut two) = (0, 0, 0, 0);
        let (mut stepped, mut escapes, mut shut_out) = (0, 0, 0);
        for seed in 0..4_000u64 {
            let root = Prng::new(seed).substream_named("driver.scenario");
            let topology = Topology::ALL[(seed % 6) as usize];
            let offers = draw(&root, topology);
            assert_eq!(offers, draw(&root, topology));
            open += offers.is_open() as u32;
            let plan = Plan::new(topology, &offers, &[3, 3]);
            refused += plan.refused as u32;
            turned += (!plan.refused && plan.turns_some_away(0)) as u32;
            two += offers.sinks.iter().any(|pad| pad.count == 2) as u32;
            let pads = || offers.srcs.iter().chain(&offers.sinks);
            stepped += pads().flat_map(|p| p.offers()).filter(|o| o.step > 0).count();
            escapes += pads().filter(|p| p.offers().contains(&Offer::BYTES)).count();
            let both = offers.srcs[0].offers().contains(&Offer::BYTES)
                && offers.sinks[0].offers().contains(&Offer::BYTES);
            shut_out += (plan.refused && both && topology == Topology::Line) as u32;
            for pad in offers.srcs.iter().chain(&offers.sinks) {
                assert!(pad.offers().iter().all(|o| o.ids().iter().all(|id| *id != 0)));
            }
        }
        assert!((2_800..3_200).contains(&open), "three in four are open, {open}");
        assert!(refused > 150 && turned > 100 && two > 100, "{refused} {turned} {two}");
        assert!(
            stepped > 100 && escapes > 200 && shut_out > 0,
            "{stepped} ranges, {escapes} escapes, {shut_out} shut"
        );
    }

    #[test]
    fn offers_print_per_pad() {
        let mut offers = Offers::OPEN;
        offers.sinks[0] = pad(&[offer(0, &[20, 10]), offer(1, &[])]);
        let text = format!("{}", Listed { offers: &offers, streams: 1, sinks: 1 });
        assert_eq!(
            text,
            "  stream 0 offers open, changes to id 20\n  \
             sink 0 offers sim/media ids [20, 10] or sim/other any id\n"
        );
        assert_eq!(format!("{}", Fixed { family: 1, id: 0 }), "sim/other with the id open");
    }
}
