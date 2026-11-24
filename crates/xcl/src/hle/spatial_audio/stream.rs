//! Timing and disposable mix buffers for the silent spatial renderer.

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex, Weak};

use windows::core::{BOOL, Error, GUID, Result, implement};
use windows::Win32::Foundation::{E_INVALIDARG, E_NOINTERFACE, E_POINTER};
use windows::Win32::Media::Audio::*;
use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, FILETIME, HANDLE,
};
use windows_sys::Win32::System::Threading::{
    CloseThreadpoolTimer, CreateThreadpoolTimer, GetCurrentProcess, PTP_CALLBACK_INSTANCE,
    PTP_TIMER, ResetEvent, SetEvent, SetThreadpoolTimer, WaitForThreadpoolTimerCallbacks,
};

const PERIOD_MS: u32 = 10;
pub(super) const FRAMES_PER_BUFFER: u32 = super::SAMPLE_RATE * PERIOD_MS / 1000;
pub(super) const MAX_DYNAMIC_OBJECTS: u32 = 128;
type Buffer = [f32; FRAMES_PER_BUFFER as usize];

struct BufferClock {
    event: HANDLE,
    timer: PTP_TIMER,
}

unsafe extern "system" fn signal_buffer(
    _instance: PTP_CALLBACK_INSTANCE, event: *mut c_void, _timer: PTP_TIMER,
) {
    // The timer owns the duplicate until cancellation has drained every callback.
    unsafe { SetEvent(event) };
}

impl BufferClock {
    fn new(event: HANDLE) -> Result<Self> {
        if event.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let mut duplicate = null_mut();
        unsafe {
            let process = GetCurrentProcess();
            if DuplicateHandle(process, event, process, &mut duplicate, 0, 0, DUPLICATE_SAME_ACCESS) == 0 {
                return Err(Error::from_thread());
            }
            let timer = CreateThreadpoolTimer(Some(signal_buffer), duplicate, null());
            if timer == 0 {
                let err = Error::from_thread();
                CloseHandle(duplicate);
                return Err(err);
            }
            Ok(Self { event: duplicate, timer })
        }
    }

    fn start(&self) {
        // Negative FILETIME values are relative. Request the first buffer immediately.
        let due = FILETIME { dwLowDateTime: u32::MAX, dwHighDateTime: u32::MAX };
        unsafe { SetThreadpoolTimer(self.timer, &due, PERIOD_MS, 0) };
    }

    fn cancel(&self) {
        unsafe {
            SetThreadpoolTimer(self.timer, null(), 0, 0);
            WaitForThreadpoolTimerCallbacks(self.timer, 1);
        }
    }

    fn stop(&self) {
        self.cancel();
        unsafe { ResetEvent(self.event) };
    }
}

impl Drop for BufferClock {
    fn drop(&mut self) {
        self.cancel();
        unsafe {
            CloseThreadpoolTimer(self.timer);
            CloseHandle(self.event);
        }
    }
}

#[derive(Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Stopped,
    Ready,
    Updating,
}

struct ObjectState {
    kind: AudioObjectType,
    buffer: BufferState,
}

#[derive(PartialEq, Eq)]
enum BufferState {
    New,
    Written(u64),
    Ending,
    Inactive,
}

#[derive(Default)]
struct StreamState {
    phase: Phase,
    pass: u64,
    next_object: u64,
    objects: HashMap<u64, ObjectState>,
}

impl StreamState {
    fn dynamic_slots_left(&self, max: u32) -> u32 {
        max - self.objects.values().filter(|object| object.kind == AudioObjectType_Dynamic).count() as u32
    }

    fn updating(&self) -> Result<()> {
        if self.phase != Phase::Updating {
            return Err(SPTLAUDCLNT_E_OUT_OF_ORDER.into());
        }
        Ok(())
    }

    fn updating_object(&mut self, id: u64) -> Result<&mut ObjectState> {
        self.updating()?;
        self.objects.get_mut(&id)
            .filter(|object| object.buffer != BufferState::Inactive)
            .ok_or_else(|| SPTLAUDCLNT_E_RESOURCES_INVALIDATED.into())
    }
}

#[implement(ISpatialAudioObjectRenderStream)]
pub(super) struct SilentStream {
    clock: BufferClock,
    state: Arc<Mutex<StreamState>>,
    static_mask: AudioObjectType,
    max_dynamic: u32,
}

impl SilentStream {
    pub(super) fn new(params: &SpatialAudioObjectRenderStreamActivationParams) -> Result<Self> {
        // Dynamic capacity is fixed, so a capacity-change notification is never needed.
        Ok(Self {
            clock: BufferClock::new(params.EventHandle.0)?,
            state: Arc::new(Mutex::new(StreamState::default())),
            static_mask: params.StaticObjectTypeMask,
            max_dynamic: params.MaxDynamicObjectCount,
        })
    }
}

impl ISpatialAudioObjectRenderStreamBase_Impl for SilentStream_Impl {
    fn GetAvailableDynamicObjectCount(&self) -> Result<u32> {
        Ok(self.state.lock().unwrap().dynamic_slots_left(self.max_dynamic))
    }

    fn GetService(&self, _iid: *const GUID, service: *mut *mut c_void) -> Result<()> {
        let service = unsafe { service.as_mut() }.ok_or(E_POINTER)?;
        *service = null_mut();
        Err(E_NOINTERFACE.into())
    }

    fn Start(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.phase != Phase::Stopped {
            return Err(SPTLAUDCLNT_E_STREAM_NOT_STOPPED.into());
        }
        state.phase = Phase::Ready;
        self.clock.start();
        Ok(())
    }

    fn Stop(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.phase != Phase::Stopped {
            self.clock.stop();
            state.phase = Phase::Stopped;
        }
        Ok(())
    }

    fn Reset(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.phase != Phase::Stopped {
            return Err(SPTLAUDCLNT_E_STREAM_NOT_STOPPED.into());
        }
        state.objects.clear();
        Ok(())
    }

    fn BeginUpdatingAudioObjects(&self, available: *mut u32, frames: *mut u32) -> Result<()> {
        if available.is_null() || frames.is_null() {
            return Err(E_POINTER.into());
        }
        let mut state = self.state.lock().unwrap();
        if state.phase != Phase::Ready {
            return Err(SPTLAUDCLNT_E_OUT_OF_ORDER.into());
        }
        state.phase = Phase::Updating;
        state.pass = state.pass.wrapping_add(1);
        unsafe {
            available.write(state.dynamic_slots_left(self.max_dynamic));
            frames.write(FRAMES_PER_BUFFER);
        }
        Ok(())
    }

    fn EndUpdatingAudioObjects(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state.updating()?;
        let pass = state.pass;
        // An object that starts writing must write in every pass. After it ends,
        // its slot remains reserved until the guest releases the COM object.
        for object in state.objects.values_mut() {
            if object.buffer == BufferState::Ending
                || matches!(object.buffer, BufferState::Written(last) if last != pass)
            {
                object.buffer = BufferState::Inactive;
            }
        }
        state.phase = Phase::Ready;
        Ok(())
    }
}

impl ISpatialAudioObjectRenderStream_Impl for SilentStream_Impl {
    fn ActivateSpatialAudioObject(&self, kind: AudioObjectType) -> Result<ISpatialAudioObject> {
        let mut state = self.state.lock().unwrap();
        if kind == AudioObjectType_Dynamic {
            if state.dynamic_slots_left(self.max_dynamic) == 0 {
                return Err(SPTLAUDCLNT_E_NO_MORE_OBJECTS.into());
            }
        } else {
            if kind != AudioObjectType_None
                && (kind.0.count_ones() != 1 || kind.0 & self.static_mask.0 == 0)
            {
                return Err(SPTLAUDCLNT_E_STATIC_OBJECT_NOT_AVAILABLE.into());
            }
            if state.objects.values().any(|object| object.kind == kind) {
                return Err(SPTLAUDCLNT_E_OBJECT_ALREADY_ACTIVE.into());
            }
        }
        let id = state.next_object;
        state.next_object += 1;
        state.objects.insert(id, ObjectState { kind, buffer: BufferState::New });
        Ok(SilentObject {
            kind,
            id,
            state: Arc::downgrade(&self.state),
            buffer: UnsafeCell::new([0.0; FRAMES_PER_BUFFER as usize]),
        }.into())
    }
}

#[implement(ISpatialAudioObject)]
struct SilentObject {
    kind: AudioObjectType,
    id: u64,
    state: Weak<Mutex<StreamState>>,
    buffer: UnsafeCell<Buffer>,
}

impl SilentObject {
    fn stream(&self) -> Result<Arc<Mutex<StreamState>>> {
        self.state.upgrade().ok_or_else(|| SPTLAUDCLNT_E_RESOURCES_INVALIDATED.into())
    }

    fn check_active(&self) -> Result<()> {
        let state = self.stream()?;
        let state = state.lock().unwrap();
        if !state.objects.get(&self.id).is_some_and(|object| object.buffer != BufferState::Inactive) {
            return Err(SPTLAUDCLNT_E_RESOURCES_INVALIDATED.into());
        }
        Ok(())
    }
}

impl Drop for SilentObject {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state.lock().unwrap().objects.remove(&self.id);
        }
    }
}

impl ISpatialAudioObjectBase_Impl for SilentObject_Impl {
    fn GetBuffer(&self, buffer: *mut *mut u8, bytes: *mut u32) -> Result<()> {
        if buffer.is_null() || bytes.is_null() {
            return Err(E_POINTER.into());
        }
        let state = self.stream()?;
        let mut state = state.lock().unwrap();
        let pass = state.pass;
        let object = state.updating_object(self.id)?;
        if object.buffer != BufferState::Ending {
            object.buffer = BufferState::Written(pass);
        }
        // The guest fills this scratch buffer before ending the pass. The renderer
        // only tracks its lifetime, since there is no host device to play it.
        unsafe {
            buffer.write(self.buffer.get().cast());
            bytes.write(size_of::<Buffer>() as u32);
        }
        Ok(())
    }

    fn SetEndOfStream(&self, frames: u32) -> Result<()> {
        if frames > FRAMES_PER_BUFFER {
            return Err(E_INVALIDARG.into());
        }
        let state = self.stream()?;
        let mut state = state.lock().unwrap();
        state.updating_object(self.id)?.buffer = BufferState::Ending;
        Ok(())
    }

    fn IsActive(&self) -> Result<BOOL> {
        Ok(self.check_active().is_ok().into())
    }

    fn GetAudioObjectType(&self) -> Result<AudioObjectType> {
        Ok(self.kind)
    }
}

impl ISpatialAudioObject_Impl for SilentObject_Impl {
    fn SetPosition(&self, _x: f32, _y: f32, _z: f32) -> Result<()> {
        if self.kind != AudioObjectType_Dynamic {
            return Err(SPTLAUDCLNT_E_PROPERTY_NOT_SUPPORTED.into());
        }
        self.check_active()
    }

    fn SetVolume(&self, _volume: f32) -> Result<()> {
        self.check_active()
    }
}
