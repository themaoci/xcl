//! Guest-only spatial audio fallback for hosts without an active render endpoint (#695).

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;

use windows::core::{GUID, HRESULT, Interface, PCWSTR, PWSTR, Ref, Result, implement, w};
use windows::Win32::Foundation::{
    E_INVALIDARG, E_NOINTERFACE, E_OUTOFMEMORY, E_POINTER, ERROR_NOT_FOUND, PROPERTYKEY,
};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CLSCTX, CoTaskMemAlloc, STGM};
use windows::Win32::System::Variant::{VT_BLOB, VT_LPWSTR};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, PSCreateMemoryPropertyStore};

mod stream;
#[cfg(test)]
mod tests;

use stream::{FRAMES_PER_BUFFER, MAX_DYNAMIC_OBJECTS, SilentStream};

const ENDPOINT_ID: PCWSTR = w!("{0.0.0.00000000}.{A6CC38B1-1B18-4C4A-8ECC-739984D91271}");
const FRIENDLY_NAME: PCWSTR = w!("XCL Silent Spatial Audio");
const FRIENDLY_NAME_KEY: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};
const NO_ENDPOINT: HRESULT = HRESULT::from_win32(ERROR_NOT_FOUND.0);

pub(super) fn exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![CoCreateInstance]
}

#[allow(non_snake_case)] // Windows export name
unsafe extern "system" fn CoCreateInstance(
    clsid: *const GUID,
    outer: *mut c_void,
    context: u32,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let hr = HRESULT(unsafe {
        windows_sys::Win32::System::Com::CoCreateInstance(
            clsid.cast(), outer, context, iid.cast(), out,
        )
    });
    if hr.is_ok()
        && outer.is_null()
        && unsafe { clsid.as_ref() } == Some(&MMDeviceEnumerator)
        && unsafe { iid.as_ref() } == Some(&IMMDeviceEnumerator::IID)
        && !out.is_null()
        && !unsafe { *out }.is_null()
    {
        // Take ownership of the reference returned by Windows and transfer it (or
        // the replacement) back to the guest with the same COM ownership rules.
        let native = unsafe { IMMDeviceEnumerator::from_raw(*out) };
        unsafe { out.write(with_silent_endpoint(native).into_raw()) };
    }
    hr
}

fn no_active_render_device(native: &IMMDeviceEnumerator) -> bool {
    matches!(
        unsafe { native.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
            .and_then(|devices| unsafe { devices.GetCount() }),
        Ok(0)
    )
}

fn with_silent_endpoint(native: IMMDeviceEnumerator) -> IMMDeviceEnumerator {
    if no_active_render_device(&native) {
        SilentDeviceEnumerator { native, silent: SilentDevice.into() }.into()
    } else {
        native
    }
}

#[implement(IMMDeviceEnumerator)]
struct SilentDeviceEnumerator {
    native: IMMDeviceEnumerator,
    silent: IMMDevice,
}

impl IMMDeviceEnumerator_Impl for SilentDeviceEnumerator_Impl {
    fn EnumAudioEndpoints(&self, flow: EDataFlow, state: DEVICE_STATE) -> Result<IMMDeviceCollection> {
        let native = unsafe { self.native.EnumAudioEndpoints(flow, state) }?;
        if (flow != eRender && flow != eAll) || state.0 & DEVICE_STATE_ACTIVE.0 == 0 {
            return Ok(native);
        }
        let count = unsafe { native.GetCount() }?;
        let can_add_silent = if flow == eRender && state == DEVICE_STATE_ACTIVE {
            count == 0
        } else {
            no_active_render_device(&self.native)
        };
        if can_add_silent {
            Ok(SilentDeviceCollection { native, native_count: count, silent: self.silent.clone() }.into())
        } else {
            Ok(native)
        }
    }

    fn GetDefaultAudioEndpoint(&self, flow: EDataFlow, role: ERole) -> Result<IMMDevice> {
        match unsafe { self.native.GetDefaultAudioEndpoint(flow, role) } {
            Err(err) if flow == eRender && err.code() == NO_ENDPOINT
                && no_active_render_device(&self.native) => Ok(self.silent.clone()),
            result => result,
        }
    }

    fn GetDevice(&self, id: &PCWSTR) -> Result<IMMDevice> {
        if !id.is_null() && unsafe { id.as_wide() == ENDPOINT_ID.as_wide() } {
            return Ok(self.silent.clone());
        }
        unsafe { self.native.GetDevice(*id) }
    }

    fn RegisterEndpointNotificationCallback(&self, client: Ref<IMMNotificationClient>) -> Result<()> {
        unsafe { self.native.RegisterEndpointNotificationCallback(client.ok()?) }
    }

    fn UnregisterEndpointNotificationCallback(&self, client: Ref<IMMNotificationClient>) -> Result<()> {
        unsafe { self.native.UnregisterEndpointNotificationCallback(client.ok()?) }
    }
}

#[implement(IMMDeviceCollection)]
struct SilentDeviceCollection {
    native: IMMDeviceCollection,
    native_count: u32,
    silent: IMMDevice,
}

impl IMMDeviceCollection_Impl for SilentDeviceCollection_Impl {
    fn GetCount(&self) -> Result<u32> {
        Ok(self.native_count + 1)
    }

    fn Item(&self, index: u32) -> Result<IMMDevice> {
        if index == self.native_count {
            return Ok(self.silent.clone());
        }
        unsafe { self.native.Item(index) }
    }
}

#[implement(IMMDevice, IMMEndpoint)]
struct SilentDevice;

impl IMMDevice_Impl for SilentDevice_Impl {
    fn Activate(
        &self, iid: *const GUID, _context: CLSCTX, _params: *const PROPVARIANT, out: *mut *mut c_void,
    ) -> Result<()> {
        let out = unsafe { out.as_mut() }.ok_or(E_POINTER)?;
        *out = null_mut();
        if unsafe { iid.as_ref() }.ok_or(E_POINTER)? != &ISpatialAudioClient::IID {
            return Err(E_NOINTERFACE.into());
        }
        let client: ISpatialAudioClient = SilentSpatialClient.into();
        *out = client.into_raw();
        crate::xlog_info!("spatial audio: using XCL's silent renderer");
        Ok(())
    }

    fn OpenPropertyStore(&self, _access: STGM) -> Result<IPropertyStore> {
        // The property store is transient. The guest can query the endpoint's name.
        unsafe {
            let mut raw = null_mut();
            PSCreateMemoryPropertyStore(&IPropertyStore::IID, &mut raw)?;
            let store = IPropertyStore::from_raw(raw);
            let mut value = PROPVARIANT::default();
            let string = copy_com_string(FRIENDLY_NAME)?;
            let inner = &mut *value.Anonymous.Anonymous;
            inner.vt = VT_LPWSTR;
            inner.Anonymous.pwszVal = string;
            store.SetValue(&FRIENDLY_NAME_KEY, &value)?;
            Ok(store)
        }
    }

    fn GetId(&self) -> Result<PWSTR> {
        copy_com_string(ENDPOINT_ID)
    }

    fn GetState(&self) -> Result<DEVICE_STATE> {
        Ok(DEVICE_STATE_ACTIVE)
    }
}

impl IMMEndpoint_Impl for SilentDevice_Impl {
    fn GetDataFlow(&self) -> Result<EDataFlow> {
        Ok(eRender)
    }
}

fn copy_com_string(string: PCWSTR) -> Result<PWSTR> {
    // Both callers pass static, nul-terminated strings. The guest owns the copy.
    let words = unsafe { string.as_wide() }.len() + 1;
    let dest = unsafe { CoTaskMemAlloc(words * size_of::<u16>()) }.cast::<u16>();
    if dest.is_null() {
        return Err(E_OUTOFMEMORY.into());
    }
    unsafe { string.0.copy_to_nonoverlapping(dest, words) };
    Ok(PWSTR(dest))
}

const SAMPLE_RATE: u32 = 48_000;
const SAMPLE_BYTES: u16 = size_of::<f32>() as u16;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;
const IEEE_FLOAT_SUBFORMAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
const FLOAT_FORMAT: WAVEFORMATEX = WAVEFORMATEX {
    wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
    nChannels: 1,
    nSamplesPerSec: SAMPLE_RATE,
    nAvgBytesPerSec: SAMPLE_RATE * SAMPLE_BYTES as u32,
    nBlockAlign: SAMPLE_BYTES,
    wBitsPerSample: SAMPLE_BYTES * 8,
    cbSize: 0,
};

#[implement(IAudioFormatEnumerator)]
struct SilentAudioFormat(UnsafeCell<WAVEFORMATEX>);

impl IAudioFormatEnumerator_Impl for SilentAudioFormat_Impl {
    fn GetCount(&self) -> Result<u32> {
        Ok(1)
    }

    fn GetFormat(&self, index: u32) -> Result<*mut WAVEFORMATEX> {
        if index != 0 {
            return Err(E_INVALIDARG.into());
        }
        // The enumerator owns the format for as long as the guest keeps it alive.
        Ok(self.0.get())
    }
}

const STATIC_SPEAKERS: [(AudioObjectType, [f32; 3]); 12] = [
    (AudioObjectType_FrontLeft, [-1.0, 0.0, -1.0]),
    (AudioObjectType_FrontRight, [1.0, 0.0, -1.0]),
    (AudioObjectType_FrontCenter, [0.0, 0.0, -1.0]),
    (AudioObjectType_LowFrequency, [0.0, 0.0, -1.0]),
    (AudioObjectType_SideLeft, [-1.0, 0.0, 0.0]),
    (AudioObjectType_SideRight, [1.0, 0.0, 0.0]),
    (AudioObjectType_BackLeft, [-1.0, 0.0, 1.0]),
    (AudioObjectType_BackRight, [1.0, 0.0, 1.0]),
    (AudioObjectType_TopFrontLeft, [-1.0, 1.0, -1.0]),
    (AudioObjectType_TopFrontRight, [1.0, 1.0, -1.0]),
    (AudioObjectType_TopBackLeft, [-1.0, 1.0, 1.0]),
    (AudioObjectType_TopBackRight, [1.0, 1.0, 1.0]),
];

#[implement(ISpatialAudioClient)]
struct SilentSpatialClient;

impl ISpatialAudioClient_Impl for SilentSpatialClient_Impl {
    fn GetStaticObjectPosition(
        &self, kind: AudioObjectType, x: *mut f32, y: *mut f32, z: *mut f32,
    ) -> Result<()> {
        if x.is_null() || y.is_null() || z.is_null() {
            return Err(E_POINTER.into());
        }
        let (_, position) = STATIC_SPEAKERS.iter().find(|(speaker, _)| *speaker == kind)
            .ok_or(SPTLAUDCLNT_E_STATIC_OBJECT_NOT_AVAILABLE)?;
        unsafe {
            x.write(position[0]);
            y.write(position[1]);
            z.write(position[2]);
        }
        Ok(())
    }

    fn GetNativeStaticObjectTypeMask(&self) -> Result<AudioObjectType> {
        Ok(AudioObjectType(STATIC_SPEAKERS.iter().fold(0, |mask, (kind, _)| mask | kind.0)))
    }

    fn GetMaxDynamicObjectCount(&self) -> Result<u32> {
        Ok(MAX_DYNAMIC_OBJECTS)
    }

    fn GetSupportedAudioObjectFormatEnumerator(&self) -> Result<IAudioFormatEnumerator> {
        Ok(SilentAudioFormat(UnsafeCell::new(FLOAT_FORMAT)).into())
    }

    fn GetMaxFrameCount(&self, format: *const WAVEFORMATEX) -> Result<u32> {
        self.IsAudioObjectFormatSupported(format)?;
        Ok(FRAMES_PER_BUFFER)
    }

    fn IsAudioObjectFormatSupported(&self, format: *const WAVEFORMATEX) -> Result<()> {
        let wave = unsafe { format.as_ref() }.ok_or(E_POINTER)?;
        if wave.nChannels != FLOAT_FORMAT.nChannels
            || wave.nSamplesPerSec != FLOAT_FORMAT.nSamplesPerSec
            || wave.nAvgBytesPerSec != FLOAT_FORMAT.nAvgBytesPerSec
            || wave.nBlockAlign != FLOAT_FORMAT.nBlockAlign
            || wave.wBitsPerSample != FLOAT_FORMAT.wBitsPerSample
        {
            return Err(AUDCLNT_E_UNSUPPORTED_FORMAT.into());
        }
        match wave.wFormatTag {
            WAVE_FORMAT_IEEE_FLOAT => Ok(()),
            WAVE_FORMAT_EXTENSIBLE
                if usize::from(wave.cbSize) >= size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>() =>
            {
                let extended = unsafe { format.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() };
                let subformat = extended.SubFormat;
                if subformat == IEEE_FLOAT_SUBFORMAT
                    && unsafe { extended.Samples.wValidBitsPerSample } == FLOAT_FORMAT.wBitsPerSample
                {
                    Ok(())
                } else {
                    Err(AUDCLNT_E_UNSUPPORTED_FORMAT.into())
                }
            }
            _ => Err(AUDCLNT_E_UNSUPPORTED_FORMAT.into()),
        }
    }

    fn IsSpatialAudioStreamAvailable(&self, iid: *const GUID, _params: *const PROPVARIANT) -> Result<()> {
        let iid = unsafe { iid.as_ref() }.ok_or(E_POINTER)?;
        if iid != &ISpatialAudioObjectRenderStream::IID {
            return Err(SPTLAUDCLNT_E_STREAM_NOT_AVAILABLE.into());
        }
        Ok(())
    }

    fn ActivateSpatialAudioStream(
        &self, params: *const PROPVARIANT, iid: *const GUID, out: *mut *mut c_void,
    ) -> Result<()> {
        let out = unsafe { out.as_mut() }.ok_or(E_POINTER)?;
        *out = null_mut();
        self.IsSpatialAudioStreamAvailable(iid, params)?;
        let params = unsafe { params.as_ref() }.ok_or(E_POINTER)?;
        let variant = unsafe { &*params.Anonymous.Anonymous };
        if variant.vt != VT_BLOB {
            return Err(E_INVALIDARG.into());
        }
        let blob = unsafe { variant.Anonymous.blob };
        let data = blob.pBlobData.cast::<SpatialAudioObjectRenderStreamActivationParams>();
        if blob.cbSize as usize != size_of::<SpatialAudioObjectRenderStreamActivationParams>()
            || data.is_null()
        {
            return Err(E_INVALIDARG.into());
        }
        let params = unsafe { &*data };
        self.IsAudioObjectFormatSupported(params.ObjectFormat)?;
        if params.MinDynamicObjectCount > params.MaxDynamicObjectCount
            || params.MaxDynamicObjectCount > MAX_DYNAMIC_OBJECTS
        {
            return Err(E_INVALIDARG.into());
        }
        if params.StaticObjectTypeMask.0 & !self.GetNativeStaticObjectTypeMask()?.0 != 0 {
            return Err(SPTLAUDCLNT_E_STATIC_OBJECT_NOT_AVAILABLE.into());
        }
        let stream: ISpatialAudioObjectRenderStream = SilentStream::new(params)?.into();
        *out = stream.into_raw();
        Ok(())
    }
}
