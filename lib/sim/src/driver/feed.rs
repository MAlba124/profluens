//! The fake source's network. The peer serves an endless pattern so each received byte is checked
//! in place, and a cancelled op that completes with bytes means data crossed a flush.

use std::time::Duration;

use profluens_core::ctx::Ctx;
use profluens_core::io::{Handle, IoResult, OpId};

use super::net::{NetDecl, Slip, pattern};
use super::pads::FakeError;
use super::wire::PAYLOAD;

pub const HOST: &str = "media.sim";
pub const PORT: u16 = 80;

/// The faults of a world are a budget far below this.
const MAX_ATTEMPTS: u32 = 64;

// What an op is, in the low byte of its tag. The rest is the feed's epoch.
const RESOLVE: u64 = 1;
const CONNECT: u64 = 2;
const SEND: u64 = 3;
const RECV: u64 = 4;
const WATCH: u64 = 5;
const BACKOFF: u64 = 6;
const QUIET: u64 = 7;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    Resolve,
    Connect,
    Ask { sent: u32, out: bool },
    Stream,
    Backoff,
    Closed,
}

pub struct Feed {
    decl: NetDecl,
    stage: Stage,
    sock: Handle,
    recv: Option<OpId>,
    watch: Option<OpId>,
    /// Bumped at every teardown. A completion of another epoch is of an op the feed gave up.
    epoch: u64,
    pos: u64,
    have: u64,
    need: u64,
    attempts: u32,
}

fn tag(kind: u64, epoch: u64) -> u64 {
    epoch << 8 | kind
}

impl Feed {
    pub fn new(decl: NetDecl) -> Feed {
        let (stage, sock, pos) = (Stage::Idle, Handle::NONE, decl.place(0));
        Feed {
            decl,
            stage,
            sock,
            recv: None,
            watch: None,
            epoch: 0,
            pos,
            have: 0,
            need: PAYLOAD as u64,
            attempts: 0,
        }
    }

    pub fn ready(&self) -> bool {
        self.have >= self.need
    }

    pub fn need(&mut self, len: usize) {
        self.need = len as u64;
    }

    pub fn spend(&mut self) {
        self.have -= self.need;
    }

    fn teardown(&mut self, ctx: &mut Ctx, cancel: bool) {
        for op in [self.recv.take().filter(|_| cancel), self.watch.take()].into_iter().flatten() {
            ctx.io().cancel(op);
        }
        if !self.sock.is_none() {
            ctx.io().close(self.sock, QUIET);
        }
        (self.sock, self.stage, self.epoch) = (Handle::NONE, Stage::Idle, self.epoch + 1);
    }

    pub fn flush(&mut self, ctx: &mut Ctx, seek_gen: u64) {
        self.teardown(ctx, self.decl.slip != Slip::NoCancel);
        (self.pos, self.have, self.attempts) = (self.decl.place(seek_gen), 0, 0);
    }

    pub fn close(&mut self, ctx: &mut Ctx) {
        if self.stage != Stage::Closed {
            self.teardown(ctx, true);
            self.stage = Stage::Closed;
        }
    }

    fn retry(&mut self, ctx: &mut Ctx) -> Result<(), FakeError> {
        self.teardown(ctx, true);
        self.attempts += 1;
        if self.attempts > MAX_ATTEMPTS {
            return Err(FakeError::NetDead { attempts: self.attempts });
        }
        ctx.io().timer(Duration::from_nanos(self.decl.backoff), tag(BACKOFF, self.epoch));
        self.stage = Stage::Backoff;
        Ok(())
    }

    fn ask(&mut self, ctx: &mut Ctx, sent: u32) {
        self.stage = Stage::Ask { sent, out: false };
        let Some(mut buf) = ctx.try_alloc() else {
            return;
        };
        let slip = (self.decl.slip == Slip::ResumeShort && self.attempts > 0) as u64;
        buf.memory.as_mut_full()[..8].copy_from_slice(&(self.pos + slip).to_le_bytes());
        buf.memory.set_len(8);
        ctx.io().send(self.sock, buf, sent, tag(SEND, self.epoch));
        self.stage = Stage::Ask { sent, out: true };
    }

    fn check(&mut self, bytes: &[u8]) -> Result<(), FakeError> {
        for (i, b) in bytes.iter().enumerate() {
            let at = self.pos + i as u64;
            if *b != pattern(self.decl.key, at) {
                return Err(FakeError::NetBytes { at });
            }
        }
        self.pos += bytes.len() as u64;
        self.have += bytes.len() as u64;
        self.attempts = 0;
        Ok(())
    }

    pub fn drain(&mut self, ctx: &mut Ctx) -> Result<(), FakeError> {
        while let Some(c) = ctx.io().next_completion() {
            let (kind, epoch) = (c.user & 0xFF, c.user >> 8);
            if epoch != self.epoch {
                // Of a connection the feed gave up and cancelled. Bytes here crossed a flush.
                if kind == RECV && matches!(c.result, IoResult::Ok(n) if n > 0) {
                    return Err(FakeError::NetStale);
                }
                continue;
            }
            match (kind, c.result) {
                (_, IoResult::Cancelled) => {}
                (RESOLVE, IoResult::Addr(addr)) => {
                    ctx.io().connect(addr, tag(CONNECT, self.epoch));
                    self.stage = Stage::Connect;
                }
                (CONNECT, IoResult::Socket { handle, .. }) => {
                    self.sock = handle;
                    self.ask(ctx, 0);
                }
                (SEND, IoResult::Ok(n)) => {
                    let Stage::Ask { sent, .. } = self.stage else {
                        continue;
                    };
                    match sent + n as u32 {
                        8.. => self.stage = Stage::Stream,
                        sent => self.ask(ctx, sent),
                    }
                }
                (RECV, IoResult::Ok(n)) => {
                    self.recv = None;
                    if let Some(op) = self.watch.take() {
                        ctx.io().cancel(op);
                    }
                    let buf = c.buf.as_ref().map_or(&[][..], |b| b.memory.data());
                    match n {
                        // The stream has no end, so a close is a lost connection.
                        0 => self.retry(ctx)?,
                        // The count is the completion's word, the buffer's length core's, the
                        // two must agree.
                        n if n != buf.len() => {
                            return Err(FakeError::NetShort { said: n, got: buf.len() });
                        }
                        _ => self.check(buf)?,
                    }
                }
                (BACKOFF, IoResult::Ok(_)) => self.stage = Stage::Idle,
                (WATCH, IoResult::Ok(_)) => {
                    self.watch = None;
                    self.retry(ctx)?;
                }
                (RESOLVE | CONNECT | SEND | RECV, IoResult::Err(_)) => self.retry(ctx)?,
                _ => {}
            }
        }
        Ok(())
    }

    pub fn pump(&mut self, ctx: &mut Ctx) {
        match self.stage {
            Stage::Idle => {
                ctx.io().resolve(HOST, PORT, 0, tag(RESOLVE, self.epoch));
                self.stage = Stage::Resolve;
            }
            Stage::Ask { sent, out: false } => self.ask(ctx, sent),
            Stage::Stream
                if self.recv.is_none()
                    && (self.have < 2 * PAYLOAD as u64 || self.have < self.need) =>
            {
                let Some(buf) = ctx.try_alloc() else { return };
                self.recv = Some(ctx.io().recv(self.sock, buf, tag(RECV, self.epoch)));
                if self.decl.slip != Slip::NoWatchdog {
                    let patience = Duration::from_nanos(self.decl.patience);
                    self.watch = Some(ctx.io().timer(patience, tag(WATCH, self.epoch)));
                }
            }
            _ => {}
        }
    }
}
