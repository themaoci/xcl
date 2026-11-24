use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Com::{BLOB, CLSCTX_INPROC_SERVER, CoTaskMemFree, STGM_READ};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::*;
use crate::pe::ImportName;
use crate::resolver::{self, Source};

#[implement(IMMDeviceEnumerator)]
struct HostDevices {
    render_active: Arc<AtomicBool>,
    fail_render_query: bool,
    capture: IMMDevice,
    render: IMMDevice,
}

impl IMMDeviceEnumerator_Impl for HostDevices_Impl {
    fn EnumAudioEndpoints(&self, flow: EDataFlow, state: DEVICE_STATE) -> Result<IMMDeviceCollection> {
        if flow == eRender && self.fail_render_query {
            return Err(E_FAIL.into());
        }
        let mut devices = Vec::new();
        if state.0 & DEVICE_STATE_ACTIVE.0 != 0 {
            if flow == eCapture || flow == eAll {
                devices.push(self.capture.clone());
            }
            if (flow == eRender || flow == eAll) && self.render_active.load(Ordering::Relaxed) {
                devices.push(self.render.clone());
            }
        }
        Ok(HostCollection(devices).into())
    }

    fn GetDefaultAudioEndpoint(&self, flow: EDataFlow, _role: ERole) -> Result<IMMDevice> {
        if flow == eCapture {
            return Ok(self.capture.clone());
        }
        if flow == eRender && self.render_active.load(Ordering::Relaxed) {
            return Ok(self.render.clone());
        }
        Err(NO_ENDPOINT.into())
    }

    fn GetDevice(&self, _id: &PCWSTR) -> Result<IMMDevice> {
        Err(NO_ENDPOINT.into())
    }

    fn RegisterEndpointNotificationCallback(&self, _client: Ref<IMMNotificationClient>) -> Result<()> {
        Ok(())
    }

    fn UnregisterEndpointNotificationCallback(&self, _client: Ref<IMMNotificationClient>) -> Result<()> {
        Ok(())
    }
}

#[implement(IMMDeviceCollection)]
struct HostCollection(Vec<IMMDevice>);

impl IMMDeviceCollection_Impl for HostCollection_Impl {
    fn GetCount(&self) -> Result<u32> {
        Ok(self.0.len() as u32)
    }

    fn Item(&self, index: u32) -> Result<IMMDevice> {
        self.0.get(index as usize).cloned().ok_or_else(|| E_INVALIDARG.into())
    }
}

fn host_devices(active: &Arc<AtomicBool>, fail_render_query: bool) -> IMMDeviceEnumerator {
    HostDevices {
        render_active: active.clone(),
        fail_render_query,
        capture: SilentDevice.into(),
        render: SilentDevice.into(),
    }.into()
}

#[test]
fn routes_guest_com_activation_through_the_fallback() {
    let activation = ImportName::Name { hint: 0, name: "CoCreateInstance".into() };
    let uninitialize = ImportName::Name { hint: 0, name: "CoUninitialize".into() };
    for dll in ["ole32.dll", "combase.dll"] {
        assert_eq!(resolver::classify(dll, &activation), Source::Hle(super::super::SPATIAL_AUDIO.into()));
        let module = resolver::load_library(dll).expect("native COM module");
        assert_eq!(resolver::get_proc_address(module, &activation), Some(CoCreateInstance as *const () as usize));
        assert_eq!(resolver::get_proc_address(module, &uninitialize), resolver::system_proc_in(module, &uninitialize));
    }
    assert_eq!(
        resolver::classify("api-ms-win-core-com-l1-1-0.dll", &activation),
        Source::HleOther(super::super::SPATIAL_AUDIO.into()),
    );
}

#[test]
fn exposes_the_silent_endpoint_only_when_windows_reports_no_render_device() {
    let active = Arc::new(AtomicBool::new(false));
    let host = host_devices(&active, false);
    let wrapped = with_silent_endpoint(host.clone());
    assert_ne!(wrapped.as_raw(), host.as_raw());

    unsafe {
        let silent = wrapped.GetDefaultAudioEndpoint(eRender, eConsole).unwrap();
        let capture = host.GetDefaultAudioEndpoint(eCapture, eConsole).unwrap();
        let reported_capture = wrapped.GetDefaultAudioEndpoint(eCapture, eConsole).unwrap();
        assert_eq!(reported_capture.as_raw(), capture.as_raw());
        assert_eq!(silent.GetState().unwrap(), DEVICE_STATE_ACTIVE);
        assert_eq!(silent.cast::<IMMEndpoint>().unwrap().GetDataFlow().unwrap(), eRender);

        let id = silent.GetId().unwrap();
        let selected = wrapped.GetDevice(PCWSTR(id.0));
        CoTaskMemFree(Some(id.0.cast()));
        assert_eq!(selected.unwrap().as_raw(), silent.as_raw());
        let devices = wrapped.EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE).unwrap();
        assert_eq!(devices.GetCount().unwrap(), 2);
        assert_eq!(devices.Item(0).unwrap().as_raw(), capture.as_raw());
        assert_eq!(devices.Item(1).unwrap().as_raw(), silent.as_raw());
        let disabled = wrapped.EnumAudioEndpoints(eRender, DEVICE_STATE_DISABLED).unwrap();
        assert_eq!(disabled.GetCount().unwrap(), 0);

        let store = silent.OpenPropertyStore(STGM_READ).unwrap();
        let name = store.GetValue(&FRIENDLY_NAME_KEY).unwrap();
        let value = &*name.Anonymous.Anonymous;
        assert_eq!(value.vt, VT_LPWSTR);
        assert_eq!(value.Anonymous.pwszVal.as_wide(), FRIENDLY_NAME.as_wide());

        active.store(true, Ordering::Relaxed);
        let native = host.GetDefaultAudioEndpoint(eRender, eConsole).unwrap();
        assert_eq!(wrapped.GetDefaultAudioEndpoint(eRender, eConsole).unwrap().as_raw(), native.as_raw());
        assert_eq!(wrapped.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE).unwrap().GetCount().unwrap(), 1);
        assert_eq!(with_silent_endpoint(host.clone()).as_raw(), host.as_raw());
    }

    let host = host_devices(&active, true);
    assert_eq!(with_silent_endpoint(host.clone()).as_raw(), host.as_raw());
}

struct TestEvent(HANDLE);

impl TestEvent {
    fn new() -> Self {
        Self(unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.unwrap())
    }

    fn wait_for_buffer(&self) {
        assert_eq!(unsafe { WaitForSingleObject(self.0, 2_000) }, WAIT_OBJECT_0);
    }
}

impl Drop for TestEvent {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn activate_stream(
    client: &ISpatialAudioClient, params: &SpatialAudioObjectRenderStreamActivationParams,
) -> Result<ISpatialAudioObjectRenderStream> {
    // The BLOB borrows the caller's params. PROPVARIANT's Drop would free them.
    let mut blob = ManuallyDrop::new(PROPVARIANT::default());
    unsafe {
        let variant = &mut *blob.Anonymous.Anonymous;
        variant.vt = VT_BLOB;
        variant.Anonymous.blob = BLOB {
            cbSize: size_of::<SpatialAudioObjectRenderStreamActivationParams>() as u32,
            pBlobData: std::ptr::from_ref(params).cast_mut().cast(),
        };
        client.ActivateSpatialAudioStream(&*blob)
    }
}

#[test]
fn renders_repeated_silent_buffers_and_stops_signaling() {
    let event = TestEvent::new();
    let host = host_devices(&Arc::new(AtomicBool::new(false)), false);
    let wrapped = with_silent_endpoint(host);
    unsafe {
        let device = wrapped.GetDefaultAudioEndpoint(eRender, eConsole).unwrap();
        let client: ISpatialAudioClient = device.Activate(CLSCTX_INPROC_SERVER, None).unwrap();
        let formats = client.GetSupportedAudioObjectFormatEnumerator().unwrap();
        assert_eq!(formats.GetCount().unwrap(), 1);
        let format = formats.GetFormat(0).unwrap();
        assert_eq!(client.GetMaxFrameCount(format).unwrap(), 480);
        let extended = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
                ..FLOAT_FORMAT
            },
            Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: FLOAT_FORMAT.wBitsPerSample },
            dwChannelMask: 0x4, // SPEAKER_FRONT_CENTER
            SubFormat: IEEE_FLOAT_SUBFORMAT,
        };
        assert_eq!(client.GetMaxFrameCount(&raw const extended.Format).unwrap(), 480);
        assert_ne!(client.GetNativeStaticObjectTypeMask().unwrap().0 & AudioObjectType_FrontLeft.0, 0);
        let mut params = SpatialAudioObjectRenderStreamActivationParams {
            ObjectFormat: format,
            StaticObjectTypeMask: AudioObjectType_FrontLeft,
            MinDynamicObjectCount: 1,
            MaxDynamicObjectCount: 1,
            Category: AudioCategory_GameEffects,
            EventHandle: event.0,
            ..Default::default()
        };
        let stream = activate_stream(&client, &params).unwrap();
        params.MinDynamicObjectCount = 2;
        assert_eq!(activate_stream(&client, &params).unwrap_err().code(), E_INVALIDARG);
        let dynamic = stream.ActivateSpatialAudioObject(AudioObjectType_Dynamic).unwrap();
        assert_eq!(
            stream.ActivateSpatialAudioObject(AudioObjectType_Dynamic).unwrap_err().code(),
            SPTLAUDCLNT_E_NO_MORE_OBJECTS,
        );
        let front = stream.ActivateSpatialAudioObject(AudioObjectType_FrontLeft).unwrap();
        assert_eq!(
            stream.ActivateSpatialAudioObject(AudioObjectType_FrontRight).unwrap_err().code(),
            SPTLAUDCLNT_E_STATIC_OBJECT_NOT_AVAILABLE,
        );
        assert_eq!(stream.GetAvailableDynamicObjectCount().unwrap(), 0);

        stream.Start().unwrap();
        assert_eq!(stream.Start().unwrap_err().code(), SPTLAUDCLNT_E_STREAM_NOT_STOPPED);
        for pass in 0..4 {
            event.wait_for_buffer();
            let (mut available, mut frames) = (u32::MAX, 0);
            stream.BeginUpdatingAudioObjects(&mut available, &mut frames).unwrap();
            assert_eq!((available, frames), (0, 480));
            assert_eq!(
                stream.BeginUpdatingAudioObjects(&mut available, &mut frames).unwrap_err().code(),
                SPTLAUDCLNT_E_OUT_OF_ORDER,
            );
            for object in [&dynamic, &front] {
                let (mut data, mut bytes) = (null_mut(), 0);
                object.GetBuffer(&mut data, &mut bytes).unwrap();
                assert_eq!(bytes, frames * SAMPLE_BYTES as u32);
                data.cast::<f32>().write(0.5);
                data.cast::<f32>().add(frames as usize - 1).write(-0.5);
            }
            if pass == 3 {
                dynamic.SetEndOfStream(frames).unwrap();
            }
            stream.EndUpdatingAudioObjects().unwrap();
        }
        assert!(!dynamic.IsActive().unwrap().as_bool());
        assert!(front.IsActive().unwrap().as_bool());
        assert_eq!(stream.GetAvailableDynamicObjectCount().unwrap(), 0);
        drop(dynamic);
        assert_eq!(stream.GetAvailableDynamicObjectCount().unwrap(), 1);
        stream.Stop().unwrap();
        assert_eq!(WaitForSingleObject(event.0, 35), WAIT_TIMEOUT);
        stream.Reset().unwrap();
        assert!(!front.IsActive().unwrap().as_bool());

        stream.Start().unwrap();
        event.wait_for_buffer();
        stream.Stop().unwrap();
    }
}
