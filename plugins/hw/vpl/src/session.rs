//! The dispatcher loader, sessions on the shared device and runtime-allocated surfaces.

use std::ptr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicUsize, Ordering};

use crate::error::{MfxError, MfxResult, check};
use crate::ffi;
use crate::group::VplGroup;
use crate::va::Device;

pub struct Loader {
    raw: ffi::mfxLoader,
}

// SAFETY: the loader is only read once made, from any thread that creates a session.
unsafe impl Send for Loader {}
unsafe impl Sync for Loader {}

impl Loader {
    pub fn new() -> Option<Loader> {
        // SAFETY: the dispatcher's entry, no arguments.
        let raw = unsafe { ffi::MFXLoad() };
        if raw.is_null() {
            return None;
        }
        let loader = Loader { raw };
        loader.filter(b"mfxImplDescription.Impl\0", ffi::MFX_IMPL_TYPE_HARDWARE);
        loader.filter(b"mfxImplDescription.AccelerationMode\0", ffi::MFX_ACCEL_MODE_VIA_VAAPI);
        Some(loader)
    }

    fn filter(&self, name: &'static [u8], value: u32) {
        // SAFETY: the loader is live, the name is nul terminated.
        unsafe {
            let cfg = ffi::MFXCreateConfig(self.raw);
            if !cfg.is_null() {
                ffi::MFXSetConfigFilterProperty(cfg, name.as_ptr(), ffi::mfxVariant::u32(value));
            }
        }
    }

    pub fn raw(&self) -> ffi::mfxLoader {
        self.raw
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        // SAFETY: the one owner, the configs it made go with it.
        unsafe { ffi::MFXUnload(self.raw) };
    }
}

/// A session, and the group it joined, which outlives it.
pub struct Session {
    raw: ffi::mfxSession,
    pub version: ffi::mfxVersion,
    pub device: Arc<Device>,
    group: Option<Arc<VplGroup>>,
    #[cfg(test)]
    fake: Option<usize>,
}

// SAFETY: used from the owning element's thread, the dispatcher serializes the rest.
unsafe impl Send for Session {}

impl Session {
    pub fn new(loader: &Loader, device: &Arc<Device>) -> MfxResult<Session> {
        let mut raw: ffi::mfxSession = ptr::null_mut();
        // SAFETY: the loader is live, raw a valid out pointer.
        check(unsafe { ffi::MFXCreateSession(loader.raw(), 0, &mut raw) }, "MFXCreateSession")?;
        let mut s = Session {
            raw,
            version: ffi::mfxVersion::default(),
            device: Arc::clone(device),
            group: None,
            #[cfg(test)]
            fake: None,
        };
        // SAFETY: the session is live, the display outlives it.
        check(
            unsafe { ffi::MFXVideoCORE_SetHandle(raw, ffi::MFX_HANDLE_VA_DISPLAY, device.raw()) },
            "MFXVideoCORE_SetHandle",
        )?;
        // SAFETY: a valid out pointer.
        check(unsafe { ffi::MFXQueryVersion(raw, &mut s.version) }, "MFXQueryVersion")?;
        Ok(s)
    }

    pub fn raw(&self) -> ffi::mfxSession {
        self.raw
    }

    pub fn platform(&self) -> MfxResult<ffi::mfxPlatform> {
        let mut p = ffi::mfxPlatform::default();
        // SAFETY: a valid out struct.
        check(
            unsafe { ffi::MFXVideoCORE_QueryPlatform(self.raw, &mut p) },
            "MFXVideoCORE_QueryPlatform",
        )?;
        Ok(p)
    }

    /// Joins `group`'s parent, `Ok(false)` while the runtime has tasks queued, to try again.
    /// Joins and disjoins hold [`JOINS`], they come from any thread.
    pub fn join_to(&mut self, group: &Arc<VplGroup>) -> MfxResult<bool> {
        let _held = joins();
        #[cfg(test)]
        if self.fake.is_some() || group.parent().fake.is_some() {
            let st = fake::join(self.fake, group.parent().fake);
            // A real session is never joined to a fake parent, so it records no group to leave.
            if self.fake.is_none() && st == ffi::MFX_ERR_NONE {
                return Ok(true);
            }
            return self.joined(st, group);
        }
        // SAFETY: both live, the parent for as long as the group.
        let st = unsafe { ffi::MFXJoinSession(group.parent().raw, self.raw) };
        self.joined(st, group)
    }

    fn joined(&mut self, st: ffi::mfxStatus, group: &Arc<VplGroup>) -> MfxResult<bool> {
        if st == ffi::MFX_WRN_IN_EXECUTION {
            return Ok(false);
        }
        check(st, "MFXJoinSession")?;
        self.group = Some(Arc::clone(group));
        Ok(true)
    }

    pub fn is_joined(&self) -> bool {
        self.group.is_some()
    }

    /// A session with no runtime behind it, every call on it logged in [`fake`].
    #[cfg(test)]
    pub fn fake(id: usize) -> Session {
        Session {
            raw: std::ptr::null_mut(),
            version: ffi::mfxVersion::default(),
            device: Arc::new(Device::fake()),
            group: None,
            fake: Some(id),
        }
    }

    /// `Ok(false)` while `syncp` is still running.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn sync(&self, syncp: ffi::mfxSyncPoint, wait_ms: u32) -> MfxResult<bool> {
        // SAFETY: the sync point came from this session.
        let st = unsafe { ffi::MFXVideoCORE_SyncOperation(self.raw, syncp, wait_ms) };
        if st == ffi::MFX_WRN_IN_EXECUTION {
            return Ok(false);
        }
        check(st, "MFXVideoCORE_SyncOperation").map(|_| true)
    }

    /// Behind `ONEVPL_EXPERIMENTAL` in the 2.16 headers.
    pub fn memory_interface(&self) -> MfxResult<*mut ffi::mfxMemoryInterface> {
        let mut hdl: ffi::mfxHDL = ptr::null_mut();
        // SAFETY: a valid out pointer.
        check(
            unsafe {
                ffi::MFXVideoCORE_GetHandle(self.raw, ffi::MFX_HANDLE_MEMORY_INTERFACE, &mut hdl)
            },
            "MFXVideoCORE_GetHandle(memory interface)",
        )?;
        if hdl.is_null() {
            return Err(MfxError {
                status: ffi::MFX_ERR_NULL_PTR,
                call: "MFXVideoCORE_GetHandle(memory interface)",
            });
        }
        Ok(hdl.cast())
    }

    pub fn import(
        &self,
        surface: &Surface,
        component: ffi::mfxSurfaceComponent,
    ) -> MfxResult<Surface> {
        let iface = self.memory_interface()?;
        let va = surface.native_handle()?;
        // SAFETY: zero is a value of every field.
        let mut desc: ffi::mfxSurfaceVAAPI = unsafe { std::mem::zeroed() };
        desc.SurfaceInterface.Header.SurfaceType = ffi::MFX_SURFACE_TYPE_VAAPI;
        desc.SurfaceInterface.Header.SurfaceFlags = ffi::MFX_SURFACE_FLAG_IMPORT_SHARED;
        desc.SurfaceInterface.Header.StructSize =
            std::mem::size_of::<ffi::mfxSurfaceVAAPI>() as u32;
        desc.vaDisplay = self.device.raw();
        desc.vaSurfaceID = va;
        let mut out: *mut ffi::mfxFrameSurface1 = ptr::null_mut();
        // SAFETY: iface came from this session, desc outlives the call.
        let import = unsafe { (*iface).ImportFrameSurface }
            .ok_or(MfxError { status: ffi::MFX_ERR_NOT_IMPLEMENTED, call: "ImportFrameSurface" })?;
        // SAFETY: as above, out a valid out pointer.
        check(
            unsafe {
                import(iface, component, (&raw mut desc.SurfaceInterface.Header).cast(), &mut out)
            },
            "ImportFrameSurface",
        )?;
        // SAFETY: the runtime handed a surface with one reference for us.
        unsafe { Surface::from_raw(out) }
            .ok_or(MfxError { status: ffi::MFX_ERR_NULL_PTR, call: "ImportFrameSurface" })
    }
}

/// Serializes a parent's session list, an element thread joins while another drops the last
/// surface of a component and disjoins.
static JOINS: Mutex<()> = Mutex::new(());

fn joins() -> std::sync::MutexGuard<'static, ()> {
    JOINS.lock().unwrap_or_else(|e| e.into_inner())
}

impl Drop for Session {
    /// A joined child leaves its group before it closes, then the group may go.
    fn drop(&mut self) {
        #[cfg(test)]
        if let Some(id) = self.fake {
            if self.group.is_some() {
                fake::log(fake::Op::Disjoin, id);
            }
            fake::log(fake::Op::Close, id);
            return;
        }
        if self.group.is_some() {
            let _held = joins();
            // SAFETY: joined above, the component on it is closed.
            unsafe { ffi::MFXDisjoinSession(self.raw) };
        }
        // SAFETY: the one owner, closed once, its components with it.
        unsafe { ffi::MFXClose(self.raw) };
    }
}

/// What the fake sessions did, in order, for the group tests.
#[cfg(test)]
// Test support.
#[allow(clippy::disallowed_methods)]
pub mod fake {
    use std::sync::Mutex;

    use crate::ffi;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Op {
        Join,
        Disjoin,
        CloseComponent,
        Close,
    }

    static LOG: Mutex<Vec<(Op, usize)>> = Mutex::new(Vec::new());
    static BUSY: Mutex<u32> = Mutex::new(0);

    pub fn log(op: Op, id: usize) {
        LOG.lock().unwrap_or_else(|e| e.into_inner()).push((op, id));
    }

    /// The entries of `ids`, taken out of the log.
    pub fn took(ids: &[usize]) -> Vec<(Op, usize)> {
        let mut log = LOG.lock().unwrap_or_else(|e| e.into_inner());
        let (mine, rest): (Vec<_>, Vec<_>) = log.drain(..).partition(|e| ids.contains(&e.1));
        *log = rest;
        mine
    }

    /// The next `n` joins report MFX_WRN_IN_EXECUTION.
    pub fn busy_joins(n: u32) {
        *BUSY.lock().unwrap_or_else(|e| e.into_inner()) = n;
    }

    /// A real child of a fake parent takes the busy count too, and is logged nowhere.
    pub fn join(child: Option<usize>, parent: Option<usize>) -> ffi::mfxStatus {
        if parent.is_none() {
            return ffi::MFX_ERR_UNSUPPORTED;
        }
        if let Some(child) = child {
            log(Op::Join, child);
        }
        let mut busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
        if *busy > 0 {
            *busy -= 1;
            return ffi::MFX_WRN_IN_EXECUTION;
        }
        ffi::MFX_ERR_NONE
    }
}

/// Closes the component once the last surface of its pool is released. Closing with
/// surfaces still out at a consumer tears the pool from under them.
///
/// A consumer's tasks reference a producer's surfaces inside the runtime, past the handles the
/// consumer holds, and release them as they complete. So a consumer keeps every producer it read
/// from, and they close only after it closed, or once it saw nothing of its own in flight.
pub struct Owner {
    pub session: Session,
    kind: AtomicU8,
    /// Dropped after the session, so the producers close last.
    producers: Mutex<Vec<Arc<Owner>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Component {
    None = 0,
    Decoder = 1,
    DecoderVpp = 2,
    Vpp = 3,
    Encoder = 4,
}

// SAFETY: surface release is atomic in the runtime, the last reference closes once.
unsafe impl Sync for Owner {}

/// Component sessions open in the process, for tests holding a reopen to its close.
static LIVE: AtomicUsize = AtomicUsize::new(0);

pub fn live_sessions() -> usize {
    LIVE.load(Ordering::Acquire)
}

impl Owner {
    // Cold, one per component.
    #[allow(clippy::disallowed_methods)]
    pub fn new(session: Session) -> Arc<Owner> {
        LIVE.fetch_add(1, Ordering::AcqRel);
        Arc::new(Owner {
            session,
            kind: AtomicU8::new(Component::None as u8),
            producers: Mutex::new(Vec::with_capacity(2)),
        })
    }

    fn producers(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Owner>>> {
        self.producers.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `producer`'s surfaces went into this component, it stays open until this one closes or
    /// [`Owner::settle`] lets it go.
    pub fn keep(&self, producer: &Arc<Owner>) {
        if std::ptr::eq(self, Arc::as_ptr(producer)) {
            return;
        }
        let mut p = self.producers();
        if !p.iter().any(|o| Arc::ptr_eq(o, producer)) {
            p.push(Arc::clone(producer));
        }
    }

    /// Nothing this component was handed is in flight, every producer but `current` may close.
    pub fn settle(&self, current: Option<&Arc<Owner>>) {
        self.producers().retain(|o| current.is_some_and(|c| Arc::ptr_eq(o, c)));
    }

    pub fn kept(&self) -> usize {
        self.producers().len()
    }

    pub fn set(&self, c: Component) {
        self.kind.store(c as u8, Ordering::Release);
    }

    pub fn component(&self) -> Component {
        match self.kind.load(Ordering::Acquire) {
            1 => Component::Decoder,
            2 => Component::DecoderVpp,
            3 => Component::Vpp,
            4 => Component::Encoder,
            _ => Component::None,
        }
    }
}

impl Drop for Owner {
    /// The component closes here, the session then disjoins and closes in its own drop.
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::AcqRel);
        #[cfg(test)]
        if let Some(id) = self.session.fake {
            if self.component() != Component::None {
                fake::log(fake::Op::CloseComponent, id);
            }
            return;
        }
        let s = self.session.raw();
        // SAFETY: initialized on this session and this is the last reference.
        unsafe {
            match self.component() {
                Component::Decoder => ffi::MFXVideoDECODE_Close(s),
                Component::DecoderVpp => ffi::MFXVideoDECODE_VPP_Close(s),
                Component::Vpp => ffi::MFXVideoVPP_Close(s),
                Component::Encoder => ffi::MFXVideoENCODE_Close(s),
                Component::None => ffi::MFX_ERR_NONE,
            };
        }
    }
}

/// One reference of a runtime-allocated surface. Drop releases, clone adds a reference.
pub struct Surface {
    ptr: *mut ffi::mfxFrameSurface1,
    owner: Option<Arc<Owner>>,
}

// SAFETY: the runtime refcounts atomically and surfaces may cross threads.
unsafe impl Send for Surface {}

impl Surface {
    /// # Safety
    /// `ptr` holds one reference for the caller, which this takes.
    pub unsafe fn from_raw(ptr: *mut ffi::mfxFrameSurface1) -> Option<Surface> {
        unsafe {
            if ptr.is_null() || (*ptr).FrameInterface.is_null() {
                return None;
            }
            Some(Surface { ptr, owner: None })
        }
    }

    pub fn owned_by(mut self, owner: &Arc<Owner>) -> Surface {
        self.owner = Some(Arc::clone(owner));
        self
    }

    /// The component whose pool the surface came from.
    pub fn owner(&self) -> Option<&Arc<Owner>> {
        self.owner.as_ref()
    }

    pub fn raw(&self) -> *mut ffi::mfxFrameSurface1 {
        self.ptr
    }

    fn iface(&self) -> &ffi::mfxFrameSurfaceInterface {
        // SAFETY: from_raw checked the interface, the runtime keeps it for the surface's life.
        unsafe { &*(*self.ptr).FrameInterface }
    }

    pub fn info(&self) -> ffi::mfxFrameInfo {
        // SAFETY: the surface is live.
        unsafe { (*self.ptr).Info }
    }

    pub fn timestamp(&self) -> u64 {
        // SAFETY: the surface is live.
        unsafe { (*self.ptr).Data.TimeStamp }
    }

    pub fn set_timestamp(&self, ts: u64) {
        // SAFETY: the surface is live and ours to stamp.
        unsafe { (*self.ptr).Data.TimeStamp = ts };
    }

    /// The runtime's threads move the count, so it is read as an atomic.
    pub fn locked(&self) -> u16 {
        // SAFETY: the surface is live, the field is a 2 aligned u16 in a repr(C) struct.
        unsafe { AtomicU16::from_ptr(&raw mut (*self.ptr).Data.Locked) }.load(Ordering::Acquire)
    }

    /// `Ok(true)` once the operation that made the surface is done, never waits.
    pub fn ready(&self) -> MfxResult<bool> {
        self.sync(0)
    }

    pub fn frame_order(&self) -> u32 {
        // SAFETY: the surface is live.
        unsafe { (*self.ptr).Data.FrameOrder }
    }

    pub fn sync(&self, wait_ms: u32) -> MfxResult<bool> {
        let f = self
            .iface()
            .Synchronize
            .ok_or(MfxError { status: ffi::MFX_ERR_NOT_IMPLEMENTED, call: "Synchronize" })?;
        // SAFETY: the surface is live.
        let st = unsafe { f(self.ptr, wait_ms) };
        if st == ffi::MFX_WRN_IN_EXECUTION {
            return Ok(false);
        }
        check(st, "mfxFrameSurfaceInterface.Synchronize").map(|_| true)
    }

    pub fn map_read(&self) -> MfxResult<Mapped<'_>> {
        self.map(ffi::MFX_MAP_READ)
    }

    pub fn map_write(&self) -> MfxResult<Mapped<'_>> {
        self.map(ffi::MFX_MAP_WRITE)
    }

    fn map(&self, flags: u32) -> MfxResult<Mapped<'_>> {
        let f = self
            .iface()
            .Map
            .ok_or(MfxError { status: ffi::MFX_ERR_NOT_IMPLEMENTED, call: "Map" })?;
        // SAFETY: the surface is live.
        check(unsafe { f(self.ptr, flags) }, "mfxFrameSurfaceInterface.Map")?;
        // SAFETY: mapped, the data pointers are set.
        let data = unsafe { (*self.ptr).Data };
        Ok(Mapped { surface: self, data })
    }

    pub fn native_handle(&self) -> MfxResult<ffi::VASurfaceID> {
        let f = self
            .iface()
            .GetNativeHandle
            .ok_or(MfxError { status: ffi::MFX_ERR_NOT_IMPLEMENTED, call: "GetNativeHandle" })?;
        let mut hdl: ffi::mfxHDL = ptr::null_mut();
        let mut kind: ffi::mfxResourceType = 0;
        // SAFETY: valid out pointers.
        check(
            unsafe { f(self.ptr, &mut hdl, &mut kind) },
            "mfxFrameSurfaceInterface.GetNativeHandle",
        )?;
        if kind != ffi::MFX_RESOURCE_VA_SURFACE_PTR || hdl.is_null() {
            return Err(MfxError {
                status: ffi::MFX_ERR_UNSUPPORTED,
                call: "GetNativeHandle(resource type)",
            });
        }
        // SAFETY: the handle is a pointer to the VASurfaceID the runtime keeps.
        Ok(unsafe { *hdl.cast::<ffi::VASurfaceID>() })
    }

    pub fn device_handle(&self) -> MfxResult<ffi::VADisplay> {
        let f = self
            .iface()
            .GetDeviceHandle
            .ok_or(MfxError { status: ffi::MFX_ERR_NOT_IMPLEMENTED, call: "GetDeviceHandle" })?;
        let mut hdl: ffi::mfxHDL = ptr::null_mut();
        let mut kind: ffi::mfxHandleType = 0;
        // SAFETY: valid out pointers.
        check(
            unsafe { f(self.ptr, &mut hdl, &mut kind) },
            "mfxFrameSurfaceInterface.GetDeviceHandle",
        )?;
        if kind != ffi::MFX_HANDLE_VA_DISPLAY {
            return Err(MfxError {
                status: ffi::MFX_ERR_UNSUPPORTED,
                call: "GetDeviceHandle(handle type)",
            });
        }
        Ok(hdl)
    }

    pub fn ref_count(&self) -> MfxResult<u32> {
        let f = self
            .iface()
            .GetRefCounter
            .ok_or(MfxError { status: ffi::MFX_ERR_NOT_IMPLEMENTED, call: "GetRefCounter" })?;
        let mut n = 0;
        // SAFETY: a valid out pointer.
        check(unsafe { f(self.ptr, &mut n) }, "mfxFrameSurfaceInterface.GetRefCounter")?;
        Ok(n)
    }
}

impl Clone for Surface {
    fn clone(&self) -> Surface {
        if let Some(f) = self.iface().AddRef {
            // SAFETY: the surface is live, the new handle owns the reference added.
            unsafe { f(self.ptr) };
        }
        Surface { ptr: self.ptr, owner: self.owner.clone() }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        if let Some(f) = self.iface().Release {
            // SAFETY: the reference this handle owned, released once.
            unsafe { f(self.ptr) };
        }
    }
}

pub struct Mapped<'a> {
    surface: &'a Surface,
    data: ffi::mfxFrameData,
}

impl Mapped<'_> {
    pub fn pitch(&self) -> usize {
        (u32::from(self.data.PitchHigh) << 16 | u32::from(self.data.Pitch)) as usize
    }

    pub fn row(&self, plane: usize, row: u32, len: usize) -> Option<&[u8]> {
        let base = match plane {
            0 => self.data.Y,
            1 => self.data.UV,
            _ => return None,
        };
        let pitch = self.pitch();
        let info = self.surface.info();
        let rows =
            if plane == 0 { u32::from(info.Height) } else { u32::from(info.Height).div_ceil(2) };
        if base.is_null() || row >= rows || len > pitch {
            return None;
        }
        // SAFETY: the plane is mapped for `rows` rows of `pitch` bytes, the row is in them.
        Some(unsafe { std::slice::from_raw_parts(base.add(row as usize * pitch), len) })
    }

    pub fn row_mut(&mut self, plane: usize, row: u32, len: usize) -> Option<&mut [u8]> {
        let base = match plane {
            0 => self.data.Y,
            1 => self.data.UV,
            _ => return None,
        };
        let pitch = self.pitch();
        let info = self.surface.info();
        let rows =
            if plane == 0 { u32::from(info.Height) } else { u32::from(info.Height).div_ceil(2) };
        if base.is_null() || row >= rows || len > pitch {
            return None;
        }
        // SAFETY: as in row, mapped to write.
        Some(unsafe { std::slice::from_raw_parts_mut(base.add(row as usize * pitch), len) })
    }
}

impl Drop for Mapped<'_> {
    fn drop(&mut self) {
        if let Some(f) = self.surface.iface().Unmap {
            // SAFETY: mapped by this guard, unmapped once.
            unsafe { f(self.surface.ptr) };
        }
    }
}

/// The fused decoder's output, one surface per channel. The runtime hands the application one
/// reference per surface and one for the array, so each surface is taken out once and the array
/// released on drop, as the oneVPL decvpp sample does.
pub struct SurfaceArray {
    ptr: *mut ffi::mfxSurfaceArray,
    taken: u32,
}

// SAFETY: as Surface.
unsafe impl Send for SurfaceArray {}

impl SurfaceArray {
    /// # Safety
    /// `ptr` holds one reference for the caller.
    pub unsafe fn from_raw(ptr: *mut ffi::mfxSurfaceArray) -> Option<SurfaceArray> {
        if ptr.is_null() { None } else { Some(SurfaceArray { ptr, taken: 0 }) }
    }

    pub fn len(&self) -> usize {
        // SAFETY: the array is live.
        unsafe { (*self.ptr).NumSurfaces as usize }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Surface `i` with the reference the runtime handed out, `None` once taken.
    pub fn take(&mut self, i: usize, owner: &Arc<Owner>) -> Option<Surface> {
        if i >= self.len() || i >= 32 || self.taken & (1 << i) != 0 {
            return None;
        }
        self.taken |= 1 << i;
        // SAFETY: i is in the array, the runtime's pointers are live while it is.
        let raw = unsafe { *(*self.ptr).Surfaces.add(i) };
        // SAFETY: the application's reference to this surface, taken once.
        unsafe { Surface::from_raw(raw) }.map(|s| s.owned_by(owner))
    }
}

impl Drop for SurfaceArray {
    fn drop(&mut self) {
        // SAFETY: the array is live.
        if let Some(f) = unsafe { (*self.ptr).Release } {
            // SAFETY: our reference, released once.
            unsafe { f(self.ptr) };
        }
    }
}

pub const EXT_MAX: usize = 8;

/// The list and the buffers must outlive every call made with `param`.
pub fn attach(param: &mut ffi::mfxVideoParam, list: &mut [*mut ffi::mfxExtBuffer]) {
    param.ExtParam = if list.is_empty() { ptr::null_mut() } else { list.as_mut_ptr() };
    param.NumExtParam = list.len() as u16;
}

pub fn header<T>(id: u32) -> ffi::mfxExtBuffer {
    ffi::mfxExtBuffer { BufferId: id, BufferSz: std::mem::size_of::<T>() as u32 }
}
