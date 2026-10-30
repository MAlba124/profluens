//! `filesink` writes incoming buffers to a file as positioned writes through the reactor.

use std::path::{Path, PathBuf};

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{Constraint, OfferDesc};
use profluens_core::id::SinkPad;
use profluens_core::io::{Handle, IoResult, OpenMode};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

use super::error::FileError;

/// Writes an untyped byte stream.
static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];

/// The file to write, created in `start()`.
static PROPS: [PropDesc; 1] = [PropDesc { name: "path", allowed: Constraint::Any, live: false }];

// Cold, one box per registry-created element.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "filesink",
    sinks: &PADS,
    srcs: &[],
    props: &PROPS,
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(FileSink::new(""))),
    io: true,
    couplings: &[],
};

const CREATE: u64 = 1;

pub struct FileSink {
    path: PathBuf,
    file: Handle,
    started: bool,
    write_offset: u64,
    in_flight: u32,
}

impl FileSink {
    pub const IN: SinkPad = SinkPad(0);

    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            file: Handle::NONE,
            started: false,
            write_offset: 0,
            in_flight: 0,
        }
    }
}

#[cold]
fn not_started(ctx: &Ctx) -> Error {
    Error::element_failed(ctx.element(), FileError::NotStarted { element: "filesink" })
}

impl Element for FileSink {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let Some(profluens_core::format::Value::Id(id)) = ctx.prop("path")
            && let Some(s) = ctx.value_name(id)
        {
            self.path = PathBuf::from(s);
        }
        ctx.io().open(&self.path, OpenMode::Create, CREATE);
        self.started = true;
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.started {
            return Err(not_started(ctx));
        }

        loop {
            let completion = ctx.io().next_completion();
            let c = match completion {
                Some(c) => c,
                None => break,
            };
            if c.user == CREATE {
                match c.result {
                    IoResult::File { handle, .. } => {
                        self.file = handle;
                        log!(&*ctx, Level::Info, "open");
                    }
                    IoResult::Err(kind) => {
                        let path = self.path.as_path().into();
                        return Err(Error::element_failed(
                            ctx.element(),
                            FileError::Create { path, kind },
                        ));
                    }
                    _ => {}
                }
                continue;
            }
            self.in_flight -= 1;
            if let IoResult::Err(k) = c.result {
                let path = self.path.as_path().into();
                return Err(Error::element_failed(
                    ctx.element(),
                    FileError::Write { path, kind: k },
                ));
            }
        }

        let credits = ctx.io().credits();
        while !self.file.is_none() && self.in_flight < credits {
            let buf = match inputs.pop() {
                Some(b) => b,
                None => break,
            };
            let n = buf.memory.len() as u64;
            let offset = self.write_offset;
            self.write_offset += n;
            ctx.io().write(self.file, offset, buf, 0);
            self.in_flight += 1;
        }

        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        // Back-patch an indexed container's front reservation. Positioned IO makes order versus
        // in-flight appends irrelevant.
        if let Event::Eos = event {
            log!(&*ctx, Level::Info, "eos", bytes = self.write_offset, in_flight = self.in_flight);
        }
        if let Event::Patch { offset, data } = event {
            // Before the create completes there is no handle to write through.
            if !self.started || self.file.is_none() {
                return Err(not_started(ctx));
            }
            log!(&*ctx, Level::Debug, "patch", offset = *offset, bytes = data.len());
            let buf = profluens_core::buffer::Buffer {
                memory: data.clone(),
                pts: Timestamp::NONE,
                dts: Timestamp::NONE,
                duration: Timestamp::NONE,
                flags: profluens_core::buffer::BufferFlags::empty(),
                format: profluens_core::id::FormatId(0),
                sync: None,
            };
            ctx.io().write(self.file, *offset, buf, 0);
            self.in_flight += 1;
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.started = false; // the reactor closes the file when the group ends
    }
}
