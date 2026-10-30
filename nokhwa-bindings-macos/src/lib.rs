/*
* Copyright 2022 l1npengtul <l1npengtul@protonmail.com> / The Nokhwa Contributors
*
* Licensed under the Apache License, Version 2.0 (the "License");
* you may not use this file except in compliance with the License.
* You may obtain a copy of the License at
*
*     http://www.apache.org/licenses/LICENSE-2.0
*
* Unless required by applicable law or agreed to in writing, software
* distributed under the License is distributed on an "AS IS" BASIS,
* WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
* See the License for the specific language governing permissions and
* limitations under the License.
*/

// hello, future peng here
// whatever is written here will induce horrors uncomprehendable.
// save yourselves. write apple code in swift and bind it to rust.

// <some change so we can call this 0.10.4>

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod internal {
    use block2::RcBlock;
    use dispatch2::{DispatchQueue, DispatchRetained};
    use flume::{Receiver, Sender};
    use nokhwa_core::{
        error::NokhwaError,
        types::{
            ApiBackend, CameraControl, CameraFormat, CameraIndex, CameraInfo,
            ControlValueDescription, ControlValueSetter, FrameFormat, KnownCameraControl,
            KnownCameraControlFlag, Resolution,
        },
    };
    use objc2::{
        define_class, msg_send,
        rc::Retained,
        runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject},
        AllocAnyThread, DefinedClass,
    };
    use objc2_av_foundation as av;
    use objc2_av_foundation::{
        AVCaptureConnection, AVCaptureExposureDurationCurrent, AVCaptureExposureTargetBiasCurrent,
        AVCaptureISOCurrent, AVCaptureOutput, AVCaptureVideoDataOutputSampleBufferDelegate,
    };
    use objc2_core_foundation::CGPoint;
    use objc2_core_media::{
        kCMPixelFormat_24RGB, kCMPixelFormat_422YpCbCr8_yuvs,
        kCMPixelFormat_8IndexedGray_WhiteIsZero, kCMVideoCodecType_422YpCbCr8,
        kCMVideoCodecType_JPEG, kCMVideoCodecType_JPEG_OpenDML, CMSampleBuffer, CMTime,
        CMVideoDimensions, CMVideoFormatDescriptionGetDimensions,
    };
    use objc2_core_video::{
        kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer,
        CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
        CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
        CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount,
        CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane, CVPixelBufferIsPlanar,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};
    use std::{
        borrow::Cow, cmp::Ordering, collections::BTreeMap, convert::TryFrom, ffi::CStr, sync::Arc,
        time::Duration,
    };

    pub use av::{AVAuthorizationStatus, AVCaptureDevicePosition};

    type NSInteger = isize;

    extern "C" {
        fn mach_absolute_time() -> u64;
    }

    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }

    extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    }

    fn mach_absolute_time_nanos() -> u64 {
        static TIMEBASE: once_cell::sync::Lazy<(u32, u32)> = once_cell::sync::Lazy::new(|| {
            let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
            unsafe { mach_timebase_info(&mut info) };
            (info.numer, info.denom)
        });
        let ticks = unsafe { mach_absolute_time() };
        let (numer, denom) = *TIMEBASE;
        ticks.wrapping_mul(numer as u64) / (denom as u64)
    }

    fn media_type_video() -> &'static NSString {
        unsafe { av::AVMediaTypeVideo }.expect("AVMediaTypeVideo is not available")
    }

    #[allow(non_upper_case_globals)]
    fn raw_fcc_to_frameformat(raw: u32) -> Option<FrameFormat> {
        match raw {
            kCMVideoCodecType_422YpCbCr8 | kCMPixelFormat_422YpCbCr8_yuvs => {
                Some(FrameFormat::YUYV)
            }
            kCMVideoCodecType_JPEG | kCMVideoCodecType_JPEG_OpenDML => Some(FrameFormat::MJPEG),
            kCMPixelFormat_8IndexedGray_WhiteIsZero => Some(FrameFormat::GRAY),
            kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
            | kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange => Some(FrameFormat::NV12),
            kCMPixelFormat_24RGB => Some(FrameFormat::RAWRGB),
            _ => None,
        }
    }

    /// Copy locked `CVPixelBuffer` into a tightly-packed byte buffer without stride padding.
    /// CVPixelBuffers are usually padded; this converts such a strided/padded frame to non-strided.
    ///
    /// # Safety
    /// `image_buffer`'s base address must currently be locked.
    unsafe fn copy_pixel_buffer_packed(image_buffer: &CVPixelBuffer) -> Vec<u8> {
        if CVPixelBufferIsPlanar(image_buffer) {
            let plane_count = CVPixelBufferGetPlaneCount(image_buffer);
            let mut out = Vec::new();
            for plane in 0..plane_count {
                let base = CVPixelBufferGetBaseAddressOfPlane(image_buffer, plane) as *const u8;
                if base.is_null() {
                    continue;
                }
                let stride = CVPixelBufferGetBytesPerRowOfPlane(image_buffer, plane);
                let width = CVPixelBufferGetWidthOfPlane(image_buffer, plane);
                let height = CVPixelBufferGetHeightOfPlane(image_buffer, plane);
                // 8-bit 4:2:0 bi-planar (NV12) is the only planar format supported:
                // plane 0 is luma at 1 byte/pixel
                // plane 1 is interleaved Cb+Cr at 2 bytes/downsampled pixel
                let bytes_per_pixel = if plane == 0 { 1 } else { 2 };
                let valid = (width * bytes_per_pixel).min(stride);
                out.reserve(valid * height);
                for row in 0..height {
                    let row_ptr = unsafe { base.add(row * stride) };
                    out.extend_from_slice(unsafe { std::slice::from_raw_parts(row_ptr, valid) });
                }
            }
            out
        } else {
            let base = CVPixelBufferGetBaseAddress(image_buffer) as *const u8;
            if base.is_null() {
                return Vec::new();
            }
            let stride = CVPixelBufferGetBytesPerRow(image_buffer);
            let width = CVPixelBufferGetWidth(image_buffer);
            let height = CVPixelBufferGetHeight(image_buffer);
            let fcc = CVPixelBufferGetPixelFormatType(image_buffer);
            let bytes_per_row = match raw_fcc_to_frameformat(fcc) {
                Some(FrameFormat::YUYV) => width * 2,
                Some(FrameFormat::RAWRGB) => width * 3,
                Some(FrameFormat::GRAY) => width,
                _ => stride, // Don't truncate unknown formats
            }
            .min(stride);
            let mut out = Vec::with_capacity(bytes_per_row * height);
            for row in 0..height {
                out.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(base.add(row * stride), bytes_per_row)
                });
            }
            out
        }
    }

    pub type CompressionData<'a> = (Cow<'a, [u8]>, FrameFormat, Option<Duration>);
    pub type DataPipe<'a> = (Sender<CompressionData<'a>>, Receiver<CompressionData<'a>>);

    type FrameMessage = (Vec<u8>, FrameFormat, Option<Duration>);

    struct CaptureCallbackIvars {
        sender: Sender<FrameMessage>,
    }

    define_class!(
        // SAFETY:
        // - The superclass NSObject does not have any subclassing requirements.
        // - `CaptureCallback` does not implement `Drop`.
        #[unsafe(super(NSObject))]
        #[name = "NokhwaCaptureCallback"]
        #[ivars = CaptureCallbackIvars]
        struct CaptureCallback;

        unsafe impl NSObjectProtocol for CaptureCallback {}

        // Delegate compliance method
        unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for CaptureCallback {
            #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
            fn capture_output_did_output_sample_buffer(
                &self,
                _output: &AVCaptureOutput,
                sample_buffer: &CMSampleBuffer,
                _connection: &AVCaptureConnection,
            ) {
                let Some(image_buffer) = (unsafe { sample_buffer.image_buffer() }) else {
                    return;
                };
                unsafe { CVPixelBufferLockBaseAddress(&image_buffer, CVPixelBufferLockFlags(0)) };

                // Tightly pack possibly strided pixel data.
                // nokhwa's decoders expect tightly-packed rows with no padding...
                let buffer_as_vec = unsafe { copy_pixel_buffer_packed(&image_buffer) };

                unsafe { CVPixelBufferUnlockBaseAddress(&image_buffer, CVPixelBufferLockFlags(0)) };

                // CMSampleBufferGetPresentationTimeStamp returns the sensor
                // capture instant on a monotonic clock (mach_absolute_time
                // timebase).  Convert to Unix wallclock:
                //   wall = SystemTime::now() - (mach_now - pts)
                let capture_ts = {
                    let pts = unsafe { sample_buffer.presentation_time_stamp() };
                    let (pts_value, pts_timescale) = (pts.value, pts.timescale);
                    if pts_timescale > 0 {
                        let pts_nanos = (pts_value as u128).saturating_mul(1_000_000_000)
                            / (pts_timescale as u128);
                        let mono_now_nanos = mach_absolute_time_nanos() as u128;
                        let wall_now = std::time::SystemTime::now();

                        let age =
                            Duration::from_nanos(mono_now_nanos.saturating_sub(pts_nanos) as u64);
                        wall_now
                            .duration_since(std::time::UNIX_EPOCH)
                            .ok()
                            .and_then(|wall_dur| wall_dur.checked_sub(age))
                    } else {
                        None
                    }
                };

                // A send error means the receiving end is gone; nothing more to do.
                let _ = self
                    .ivars()
                    .sender
                    .send((buffer_as_vec, FrameFormat::GRAY, capture_ts));
            }
        }
    );

    impl CaptureCallback {
        fn new(sender: Sender<FrameMessage>) -> Retained<Self> {
            let this = Self::alloc().set_ivars(CaptureCallbackIvars { sender });
            unsafe { msg_send![super(this), init] }
        }
    }

    pub fn request_permission_with_callback(callback: impl Fn(bool) + Send + Sync + 'static) {
        let block = RcBlock::new(move |granted: Bool| {
            callback(granted.as_bool());
        });

        unsafe {
            av::AVCaptureDevice::requestAccessForMediaType_completionHandler(
                media_type_video(),
                &block,
            );
        }
    }

    pub fn current_authorization_status() -> AVAuthorizationStatus {
        unsafe { av::AVCaptureDevice::authorizationStatusForMediaType(media_type_video()) }
    }

    // fuck it, use deprecated APIs
    pub fn query_avfoundation() -> Result<Vec<CameraInfo>, NokhwaError> {
        Ok(AVCaptureDeviceDiscoverySession::new(vec![
            AVCaptureDeviceType::UltraWide,
            AVCaptureDeviceType::WideAngle,
            AVCaptureDeviceType::Telephoto,
            AVCaptureDeviceType::TrueDepth,
            AVCaptureDeviceType::External,
        ])?
        .devices())
    }

    fn position_name(position: AVCaptureDevicePosition) -> &'static str {
        match position {
            AVCaptureDevicePosition::Back => "Back",
            AVCaptureDevicePosition::Front => "Front",
            _ => "Unspecified",
        }
    }

    pub fn get_raw_device_info(index: CameraIndex, device: &av::AVCaptureDevice) -> CameraInfo {
        let name = unsafe { device.localizedName() }.to_string();
        let manufacturer = unsafe { device.manufacturer() }.to_string();
        let position = unsafe { device.position() };
        let lens_aperture = unsafe { device.lensAperture() };
        let device_type = unsafe { device.deviceType() }.to_string();
        let model_id = unsafe { device.modelID() }.to_string();
        let description = format!(
            "{}: {} - {}, {} f{}",
            manufacturer,
            model_id,
            device_type,
            position_name(position),
            lens_aperture
        );
        let misc = unsafe { device.uniqueID() }.to_string();

        CameraInfo::new(&name, &description, &misc, index)
    }

    #[derive(Copy, Clone, Debug, Hash, Ord, PartialOrd, Eq, PartialEq)]
    pub enum AVCaptureDeviceType {
        Dual,
        DualWide,
        Triple,
        WideAngle,
        UltraWide,
        Telephoto,
        TrueDepth,
        External,
    }

    impl AVCaptureDeviceType {
        // These are spelled out rather than read from the AVFoundation constants, as
        // some of the constants don't exist on every platform.
        fn name(self) -> &'static str {
            match self {
                AVCaptureDeviceType::Dual => "AVCaptureDeviceTypeBuiltInDualCamera",
                AVCaptureDeviceType::DualWide => "AVCaptureDeviceTypeBuiltInDualWideCamera",
                AVCaptureDeviceType::Triple => "AVCaptureDeviceTypeBuiltInTripleCamera",
                AVCaptureDeviceType::WideAngle => "AVCaptureDeviceTypeBuiltInWideAngleCamera",
                AVCaptureDeviceType::UltraWide => "AVCaptureDeviceTypeBuiltInUltraWideCamera",
                AVCaptureDeviceType::Telephoto => "AVCaptureDeviceTypeBuiltInTelephotoCamera",
                AVCaptureDeviceType::TrueDepth => "AVCaptureDeviceTypeBuiltInTrueDepthCamera",
                AVCaptureDeviceType::External => "AVCaptureDeviceTypeExternal",
            }
        }

        pub fn into_ns_str(self) -> Retained<NSString> {
            NSString::from_str(self.name())
        }
    }

    #[derive(Copy, Clone, Debug, Hash, Ord, PartialOrd, Eq, PartialEq)]
    pub enum AVMediaType {
        Audio,
        ClosedCaption,
        DepthData,
        Metadata,
        MetadataObject,
        Muxed,
        Subtitle,
        Text,
        Timecode,
        Video,
    }

    impl AVMediaType {
        pub fn into_ns_str(self) -> &'static NSString {
            let media_type = unsafe {
                match self {
                    AVMediaType::Audio => av::AVMediaTypeAudio,
                    AVMediaType::ClosedCaption => av::AVMediaTypeClosedCaption,
                    AVMediaType::DepthData => av::AVMediaTypeDepthData,
                    AVMediaType::Metadata => av::AVMediaTypeMetadata,
                    AVMediaType::MetadataObject => av::AVMediaTypeMetadataObject,
                    AVMediaType::Muxed => av::AVMediaTypeMuxed,
                    AVMediaType::Subtitle => av::AVMediaTypeSubtitle,
                    AVMediaType::Text => av::AVMediaTypeText,
                    AVMediaType::Timecode => av::AVMediaTypeTimecode,
                    AVMediaType::Video => av::AVMediaTypeVideo,
                }
            };
            media_type.expect("AVMediaType constant is not available")
        }
    }

    impl TryFrom<&NSString> for AVMediaType {
        type Error = NokhwaError;

        fn try_from(value: &NSString) -> Result<Self, Self::Error> {
            const ALL: [AVMediaType; 10] = [
                AVMediaType::Audio,
                AVMediaType::ClosedCaption,
                AVMediaType::DepthData,
                AVMediaType::Metadata,
                AVMediaType::MetadataObject,
                AVMediaType::Muxed,
                AVMediaType::Subtitle,
                AVMediaType::Text,
                AVMediaType::Timecode,
                AVMediaType::Video,
            ];
            ALL.into_iter()
                .find(|media_type| value.isEqualToString(media_type.into_ns_str()))
                .ok_or_else(|| NokhwaError::GetPropertyError {
                    property: "AVMediaType".to_string(),
                    error: format!("Invalid AVMediaType {value}"),
                })
        }
    }

    pub struct AVCaptureVideoCallback {
        delegate: Retained<CaptureCallback>,
        queue: DispatchRetained<DispatchQueue>,
    }

    impl AVCaptureVideoCallback {
        pub fn new(
            device_spec: &CStr,
            buffer: &Arc<Sender<(Vec<u8>, FrameFormat, Option<Duration>)>>,
        ) -> Result<Self, NokhwaError> {
            let delegate = CaptureCallback::new(Sender::clone(buffer));
            let queue = DispatchQueue::new(&device_spec.to_string_lossy(), None);

            Ok(AVCaptureVideoCallback { delegate, queue })
        }

        pub fn delegate(
            &self,
        ) -> &ProtocolObject<dyn AVCaptureVideoDataOutputSampleBufferDelegate> {
            ProtocolObject::from_ref(&*self.delegate)
        }

        pub fn queue(&self) -> &DispatchQueue {
            &self.queue
        }
    }

    impl Drop for AVCaptureVideoCallback {
        fn drop(&mut self) {
            // The queue is serial: once this empty block has run, no callback is still
            // using the delegate.
            self.queue.exec_sync(|| {});
        }
    }

    pub struct AVFrameRateRange {
        inner: Retained<av::AVFrameRateRange>,
    }

    impl From<Retained<av::AVFrameRateRange>> for AVFrameRateRange {
        fn from(inner: Retained<av::AVFrameRateRange>) -> Self {
            AVFrameRateRange { inner }
        }
    }

    impl AVFrameRateRange {
        pub fn inner(&self) -> &Retained<av::AVFrameRateRange> {
            &self.inner
        }

        pub fn max(&self) -> f64 {
            unsafe { self.inner.maxFrameRate() }
        }

        pub fn min(&self) -> f64 {
            unsafe { self.inner.minFrameRate() }
        }
    }

    #[derive(Debug)]
    pub struct AVCaptureDeviceFormat {
        pub(crate) internal: Retained<av::AVCaptureDeviceFormat>,
        pub resolution: CMVideoDimensions,
        pub fps_list: Vec<f64>,
        pub fourcc: FrameFormat,
    }

    impl TryFrom<Retained<av::AVCaptureDeviceFormat>> for AVCaptureDeviceFormat {
        type Error = NokhwaError;

        fn try_from(value: Retained<av::AVCaptureDeviceFormat>) -> Result<Self, Self::Error> {
            let media_type_raw = unsafe { value.mediaType() };
            let media_type = AVMediaType::try_from(&*media_type_raw)?;
            if media_type != AVMediaType::Video {
                return Err(NokhwaError::StructureError {
                    structure: "AVMediaType".to_string(),
                    error: "Not Video".to_string(),
                });
            }
            let mut fps_list = unsafe { value.videoSupportedFrameRateRanges() }
                .iter()
                .map(AVFrameRateRange::from)
                .flat_map(|v| {
                    if v.min() != 0_f64 && v.min() != 1_f64 {
                        vec![v.min(), v.max()]
                    } else {
                        vec![v.max()] // this gets deduped!
                    }
                })
                .collect::<Vec<f64>>();
            fps_list.sort_by(|n, m| n.partial_cmp(m).unwrap_or(Ordering::Equal));
            fps_list.dedup();
            let description = unsafe { value.formatDescription() };
            let resolution = unsafe { CMVideoFormatDescriptionGetDimensions(&description) };
            let fcc_raw = unsafe { description.media_sub_type() };
            #[allow(non_upper_case_globals)]
            let fourcc = match raw_fcc_to_frameformat(fcc_raw) {
                Some(fcc) => fcc,
                None => {
                    return Err(NokhwaError::StructureError {
                        structure: "FourCharCode".to_string(),
                        error: format!("Unknown FourCharCode {fcc_raw:?}"),
                    })
                }
            };

            Ok(AVCaptureDeviceFormat {
                internal: value,
                resolution,
                fps_list,
                fourcc,
            })
        }
    }

    pub struct AVCaptureDeviceDiscoverySession {
        inner: Retained<av::AVCaptureDeviceDiscoverySession>,
    }

    impl AVCaptureDeviceDiscoverySession {
        pub fn inner(&self) -> &Retained<av::AVCaptureDeviceDiscoverySession> {
            &self.inner
        }

        pub fn new(device_types: Vec<AVCaptureDeviceType>) -> Result<Self, NokhwaError> {
            let device_types = device_types
                .into_iter()
                .map(AVCaptureDeviceType::into_ns_str)
                .collect::<Vec<_>>();
            let device_types = NSArray::from_retained_slice(&device_types);

            let discovery_session = unsafe {
                av::AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
                    &device_types,
                    Some(media_type_video()),
                    AVCaptureDevicePosition::Unspecified,
                )
            };

            Ok(AVCaptureDeviceDiscoverySession {
                inner: discovery_session,
            })
        }

        pub fn default() -> Result<Self, NokhwaError> {
            AVCaptureDeviceDiscoverySession::new(vec![
                AVCaptureDeviceType::UltraWide,
                AVCaptureDeviceType::Telephoto,
                AVCaptureDeviceType::External,
                AVCaptureDeviceType::Dual,
                AVCaptureDeviceType::DualWide,
                AVCaptureDeviceType::Triple,
            ])
        }

        pub fn devices(&self) -> Vec<CameraInfo> {
            unsafe { self.inner.devices() }
                .iter()
                .enumerate()
                .map(|(index, device)| {
                    get_raw_device_info(CameraIndex::Index(index as u32), &device)
                })
                .collect()
        }
    }

    pub struct AVCaptureDevice {
        inner: Retained<av::AVCaptureDevice>,
        device: CameraInfo,
        locked: bool,
    }

    impl AVCaptureDevice {
        pub fn inner(&self) -> &Retained<av::AVCaptureDevice> {
            &self.inner
        }
    }

    impl AVCaptureDevice {
        pub fn new(index: &CameraIndex) -> Result<Self, NokhwaError> {
            match &index {
                CameraIndex::Index(idx) => {
                    let devices = query_avfoundation()?;

                    match devices.get(*idx as usize) {
                        Some(device) => Ok(AVCaptureDevice::from_id(
                            &device.misc(),
                            Some(index.clone()),
                        )?),
                        None => Err(NokhwaError::OpenDeviceError(
                            idx.to_string(),
                            "Not Found".to_string(),
                        )),
                    }
                }
                CameraIndex::String(id) => Ok(AVCaptureDevice::from_id(id, None)?),
            }
        }

        pub fn from_id(id: &str, index_hint: Option<CameraIndex>) -> Result<Self, NokhwaError> {
            let capture =
                unsafe { av::AVCaptureDevice::deviceWithUniqueID(&NSString::from_str(id)) }
                    .ok_or_else(|| {
                        NokhwaError::OpenDeviceError(id.to_string(), "Device is null".to_string())
                    })?;
            let camera_info = get_raw_device_info(
                index_hint.unwrap_or_else(|| CameraIndex::String(id.to_string())),
                &capture,
            );

            Ok(AVCaptureDevice {
                inner: capture,
                device: camera_info,
                locked: false,
            })
        }

        pub fn info(&self) -> &CameraInfo {
            &self.device
        }

        pub fn supported_formats_raw(&self) -> Result<Vec<AVCaptureDeviceFormat>, NokhwaError> {
            unsafe { self.inner.formats() }
                .iter()
                .map(AVCaptureDeviceFormat::try_from)
                .collect()
        }

        pub fn supported_formats(&self) -> Result<Vec<CameraFormat>, NokhwaError> {
            Ok(self
                .supported_formats_raw()?
                .iter()
                .flat_map(|av_fmt| {
                    let resolution = av_fmt.resolution;
                    av_fmt.fps_list.iter().map(move |fps_f64| {
                        let fps = *fps_f64 as u32;

                        let resolution =
                            Resolution::new(resolution.width as u32, resolution.height as u32); // FIXME: what the fuck?
                        CameraFormat::new(resolution, av_fmt.fourcc, fps)
                    })
                })
                .filter(|x| x.frame_rate() != 0)
                .collect())
        }

        pub fn already_in_use(&self) -> bool {
            unsafe { self.inner.isInUseByAnotherApplication() }
        }

        pub fn is_suspended(&self) -> bool {
            unsafe { self.inner.isSuspended() }
        }

        pub fn lock(&self) -> Result<(), NokhwaError> {
            if self.locked {
                return Ok(());
            }
            if self.already_in_use() {
                return Err(NokhwaError::InitializeError {
                    backend: ApiBackend::AVFoundation,
                    error: "Already in use".to_string(),
                });
            }
            unsafe { self.inner.lockForConfiguration() }.map_err(|err| {
                NokhwaError::SetPropertyError {
                    property: "lockForConfiguration".to_string(),
                    value: "Locked".to_string(),
                    error: format!(
                        "Cannot lock for configuration: {}",
                        err.localizedDescription()
                    ),
                }
            })
        }

        pub fn unlock(&mut self) {
            if self.locked {
                self.locked = false;
                unsafe { self.inner.unlockForConfiguration() }
            }
        }

        // thank you ffmpeg
        pub fn set_all(&mut self, descriptor: CameraFormat) -> Result<(), NokhwaError> {
            self.lock()?;
            let format_list = self.supported_formats_raw()?;

            let mut selected: Option<(
                Retained<av::AVCaptureDeviceFormat>,
                Retained<av::AVFrameRateRange>,
            )> = None;

            for format in format_list {
                if format.resolution.height == descriptor.resolution().height() as i32
                    && format.resolution.width == descriptor.resolution().width() as i32
                {
                    // Find a frame-rate range *on this same format* supporting
                    // the requested FPS. A device (e.g. a Logitech C930e)
                    // exposes the same resolution in several formats and supported FPSes.
                    for range in unsafe { format.internal.videoSupportedFrameRateRanges() }.iter() {
                        let max_fps = unsafe { range.maxFrameRate() };
                        // Older Apple cameras (i.e. iMac 2013) return 29.97000002997 as FPS.
                        if (f64::from(descriptor.frame_rate()) - max_fps).abs() < 0.999 {
                            selected = Some((format.internal.clone(), range));
                            break;
                        }
                    }

                    if selected.is_some() {
                        break;
                    }
                }
            }
            let Some((selected_format, selected_range)) = selected else {
                return Err(NokhwaError::SetPropertyError {
                    property: "CameraFormat".to_string(),
                    value: descriptor.to_string(),
                    error: "Not Found/Rejected/Unsupported".to_string(),
                });
            };

            unsafe {
                self.inner.setActiveFormat(&selected_format);
                let min_frame_duration = selected_range.minFrameDuration();
                self.inner
                    .setActiveVideoMinFrameDuration(min_frame_duration);
                self.inner
                    .setActiveVideoMaxFrameDuration(min_frame_duration);
            }
            self.unlock();
            Ok(())
        }

        // 0 => Focus POI
        // 1 => Focus Manual Setting
        // 2 => Exposure POI
        // 3 => Exposure Face Driven
        // 4 => Exposure Target Bias
        // 5 => Exposure ISO
        // 6 => Exposure Duration
        pub fn get_controls(&self) -> Result<Vec<CameraControl>, NokhwaError> {
            let device = &*self.inner;
            let active_format = unsafe { device.activeFormat() };

            let mut controls = vec![];
            // get focus modes

            let focus_current = unsafe { device.focusMode() }.0 as i64;
            let focus_locked =
                unsafe { device.isFocusModeSupported(av::AVCaptureFocusMode::Locked) };
            let focus_auto =
                unsafe { device.isFocusModeSupported(av::AVCaptureFocusMode::AutoFocus) };
            let focus_continuous =
                unsafe { device.isFocusModeSupported(av::AVCaptureFocusMode::ContinuousAutoFocus) };

            {
                let mut supported_focus_values = vec![];

                if focus_locked {
                    supported_focus_values.push(0)
                }
                if focus_auto {
                    supported_focus_values.push(1)
                }
                if focus_continuous {
                    supported_focus_values.push(2)
                }

                controls.push(CameraControl::new(
                    KnownCameraControl::Focus,
                    "FocusMode".to_string(),
                    ControlValueDescription::Enum {
                        value: focus_current,
                        possible: supported_focus_values,
                        default: focus_current,
                    },
                    vec![],
                    true,
                ));
            }

            let focus_poi_supported = unsafe { device.isFocusPointOfInterestSupported() };
            let focus_poi = unsafe { device.focusPointOfInterest() };

            controls.push(CameraControl::new(
                KnownCameraControl::Other(0),
                "FocusPointOfInterest".to_string(),
                ControlValueDescription::Point {
                    value: (focus_poi.x, focus_poi.y),
                    default: (0.5, 0.5),
                },
                if !focus_poi_supported {
                    vec![
                        KnownCameraControlFlag::Disabled,
                        KnownCameraControlFlag::ReadOnly,
                    ]
                } else {
                    vec![]
                },
                focus_auto || focus_continuous,
            ));

            let focus_manual = unsafe { device.isLockingFocusWithCustomLensPositionSupported() };
            let focus_lenspos = unsafe { device.lensPosition() };

            controls.push(CameraControl::new(
                KnownCameraControl::Other(1),
                "FocusManualLensPosition".to_string(),
                ControlValueDescription::FloatRange {
                    min: 0.0,
                    max: 1.0,
                    value: focus_lenspos as f64,
                    step: f64::MIN_POSITIVE,
                    default: 1.0,
                },
                if focus_manual {
                    vec![]
                } else {
                    vec![
                        KnownCameraControlFlag::Disabled,
                        KnownCameraControlFlag::ReadOnly,
                    ]
                },
                focus_manual,
            ));

            // get exposures
            let exposure_current = unsafe { device.exposureMode() }.0 as i64;
            let exposure_locked =
                unsafe { device.isExposureModeSupported(av::AVCaptureExposureMode::Locked) };
            let exposure_auto =
                unsafe { device.isExposureModeSupported(av::AVCaptureExposureMode::AutoExpose) };
            let exposure_continuous = unsafe {
                device.isExposureModeSupported(av::AVCaptureExposureMode::ContinuousAutoExposure)
            };
            let exposure_custom =
                unsafe { device.isExposureModeSupported(av::AVCaptureExposureMode::Custom) };

            {
                let mut supported_exposure_values = vec![];

                if exposure_locked {
                    supported_exposure_values.push(0);
                }
                if exposure_auto {
                    supported_exposure_values.push(1);
                }
                if exposure_continuous {
                    supported_exposure_values.push(2);
                }
                if exposure_custom {
                    supported_exposure_values.push(3);
                }

                controls.push(CameraControl::new(
                    KnownCameraControl::Exposure,
                    "ExposureMode".to_string(),
                    ControlValueDescription::Enum {
                        value: exposure_current,
                        possible: supported_exposure_values,
                        default: exposure_current,
                    },
                    vec![],
                    true,
                ));
            }

            let exposure_poi_supported = unsafe { device.isExposurePointOfInterestSupported() };
            let exposure_poi = unsafe { device.exposurePointOfInterest() };

            controls.push(CameraControl::new(
                KnownCameraControl::Other(2),
                "ExposurePointOfInterest".to_string(),
                ControlValueDescription::Point {
                    value: (exposure_poi.x, exposure_poi.y),
                    default: (0.5, 0.5),
                },
                if !exposure_poi_supported {
                    vec![
                        KnownCameraControlFlag::Disabled,
                        KnownCameraControlFlag::ReadOnly,
                    ]
                } else {
                    vec![]
                },
                focus_auto || focus_continuous,
            ));

            let expposure_face_driven_supported =
                unsafe { device.isFaceDrivenAutoExposureEnabled() };
            let exposure_face_driven =
                unsafe { device.automaticallyAdjustsFaceDrivenAutoExposureEnabled() };

            controls.push(CameraControl::new(
                KnownCameraControl::Other(3),
                "ExposureFaceDriven".to_string(),
                ControlValueDescription::Boolean {
                    value: exposure_face_driven,
                    default: false,
                },
                if !expposure_face_driven_supported {
                    vec![
                        KnownCameraControlFlag::Disabled,
                        KnownCameraControlFlag::ReadOnly,
                    ]
                } else {
                    vec![]
                },
                exposure_poi_supported,
            ));

            let exposure_bias = unsafe { device.exposureTargetBias() };
            let exposure_bias_min = unsafe { device.minExposureTargetBias() };
            let exposure_bias_max = unsafe { device.maxExposureTargetBias() };

            controls.push(CameraControl::new(
                KnownCameraControl::Other(4),
                "ExposureBiasTarget".to_string(),
                ControlValueDescription::FloatRange {
                    min: exposure_bias_min as f64,
                    max: exposure_bias_max as f64,
                    value: exposure_bias as f64,
                    step: f32::MIN_POSITIVE as f64,
                    default: unsafe { AVCaptureExposureTargetBiasCurrent } as f64,
                },
                vec![],
                true,
            ));

            let exposure_duration = unsafe { device.exposureDuration() };
            let exposure_duration_min = unsafe { active_format.minExposureDuration() };
            let exposure_duration_max = unsafe { active_format.maxExposureDuration() };

            controls.push(CameraControl::new(
                KnownCameraControl::Gamma,
                "ExposureDuration".to_string(),
                ControlValueDescription::IntegerRange {
                    min: exposure_duration_min.value,
                    max: exposure_duration_max.value,
                    value: exposure_duration.value,
                    step: 1,
                    default: unsafe { AVCaptureExposureDurationCurrent }.value,
                },
                if exposure_custom {
                    vec![
                        KnownCameraControlFlag::ReadOnly,
                        KnownCameraControlFlag::Volatile,
                    ]
                } else {
                    vec![KnownCameraControlFlag::Volatile]
                },
                exposure_custom,
            ));

            let exposure_iso = unsafe { device.ISO() };
            let exposure_iso_min = unsafe { active_format.minISO() };
            let exposure_iso_max = unsafe { active_format.maxISO() };

            controls.push(CameraControl::new(
                KnownCameraControl::Brightness,
                "ExposureISO".to_string(),
                ControlValueDescription::FloatRange {
                    min: exposure_iso_min as f64,
                    max: exposure_iso_max as f64,
                    value: exposure_iso as f64,
                    step: f32::MIN_POSITIVE as f64,
                    default: unsafe { AVCaptureISOCurrent } as f64,
                },
                if exposure_custom {
                    vec![
                        KnownCameraControlFlag::ReadOnly,
                        KnownCameraControlFlag::Volatile,
                    ]
                } else {
                    vec![KnownCameraControlFlag::Volatile]
                },
                exposure_custom,
            ));

            let lens_aperture = unsafe { device.lensAperture() };

            controls.push(CameraControl::new(
                KnownCameraControl::Iris,
                "LensAperture".to_string(),
                ControlValueDescription::Float {
                    value: lens_aperture as f64,
                    default: lens_aperture as f64,
                    step: lens_aperture as f64,
                },
                vec![KnownCameraControlFlag::ReadOnly],
                false,
            ));

            // get whiteblaance
            let white_balance_current = unsafe { device.whiteBalanceMode() }.0 as i64;
            let white_balance_manual = unsafe {
                device.isWhiteBalanceModeSupported(av::AVCaptureWhiteBalanceMode::Locked)
            };
            let white_balance_auto = unsafe {
                device.isWhiteBalanceModeSupported(av::AVCaptureWhiteBalanceMode::AutoWhiteBalance)
            };
            let white_balance_continuous = unsafe {
                device.isWhiteBalanceModeSupported(
                    av::AVCaptureWhiteBalanceMode::ContinuousAutoWhiteBalance,
                )
            };

            {
                let mut possible = vec![];

                if white_balance_manual {
                    possible.push(0);
                }
                if white_balance_auto {
                    possible.push(1);
                }
                if white_balance_continuous {
                    possible.push(2);
                }

                controls.push(CameraControl::new(
                    KnownCameraControl::WhiteBalance,
                    "WhiteBalanceMode".to_string(),
                    ControlValueDescription::Enum {
                        value: white_balance_current,
                        possible,
                        default: 0,
                    },
                    vec![],
                    true,
                ));
            }

            let white_balance_gains = unsafe { device.deviceWhiteBalanceGains() };
            let white_balance_default = unsafe { device.grayWorldDeviceWhiteBalanceGains() };
            let white_balance_max = unsafe { device.maxWhiteBalanceGain() };
            let white_balance_gain_supported =
                unsafe { device.isLockingWhiteBalanceWithCustomDeviceGainsSupported() };

            controls.push(CameraControl::new(
                KnownCameraControl::Gain,
                "WhiteBalanceGain".to_string(),
                ControlValueDescription::RGB {
                    value: (
                        white_balance_gains.redGain as f64,
                        white_balance_gains.greenGain as f64,
                        white_balance_gains.blueGain as f64,
                    ),
                    max: (
                        white_balance_max as f64,
                        white_balance_max as f64,
                        white_balance_max as f64,
                    ),
                    default: (
                        white_balance_default.redGain as f64,
                        white_balance_default.greenGain as f64,
                        white_balance_default.blueGain as f64,
                    ),
                },
                if white_balance_gain_supported {
                    vec![
                        KnownCameraControlFlag::Disabled,
                        KnownCameraControlFlag::ReadOnly,
                    ]
                } else {
                    vec![]
                },
                white_balance_gain_supported,
            ));

            // get flash
            let has_torch = unsafe { device.isTorchAvailable() };
            let torch_active = unsafe { device.isTorchActive() };
            let torch_off = unsafe { device.isTorchModeSupported(av::AVCaptureTorchMode::Off) };
            let torch_on = unsafe { device.isTorchModeSupported(av::AVCaptureTorchMode::On) };
            let torch_auto = unsafe { device.isTorchModeSupported(av::AVCaptureTorchMode::Auto) };

            {
                let mut possible = vec![];

                if torch_off {
                    possible.push(0);
                }
                if torch_on {
                    possible.push(1);
                }
                if torch_auto {
                    possible.push(2);
                }

                controls.push(CameraControl::new(
                    KnownCameraControl::Other(5),
                    "TorchMode".to_string(),
                    ControlValueDescription::Enum {
                        value: torch_active as i64,
                        possible,
                        default: 0,
                    },
                    if has_torch {
                        vec![
                            KnownCameraControlFlag::Disabled,
                            KnownCameraControlFlag::ReadOnly,
                        ]
                    } else {
                        vec![]
                    },
                    has_torch,
                ));
            }

            // get low light boost
            let has_llb = unsafe { device.isLowLightBoostSupported() };
            let llb_enabled = unsafe { device.isLowLightBoostEnabled() };

            {
                controls.push(CameraControl::new(
                    KnownCameraControl::BacklightComp,
                    "LowLightCompensation".to_string(),
                    ControlValueDescription::Boolean {
                        value: llb_enabled,
                        default: false,
                    },
                    if !has_llb {
                        vec![
                            KnownCameraControlFlag::Disabled,
                            KnownCameraControlFlag::ReadOnly,
                        ]
                    } else {
                        vec![]
                    },
                    has_llb,
                ));
            }

            // get zoom factor
            let zoom_current = unsafe { device.videoZoomFactor() };
            let zoom_min = unsafe { device.minAvailableVideoZoomFactor() };
            let zoom_max = unsafe { device.maxAvailableVideoZoomFactor() };

            controls.push(CameraControl::new(
                KnownCameraControl::Zoom,
                "Zoom".to_string(),
                ControlValueDescription::FloatRange {
                    min: zoom_min,
                    max: zoom_max,
                    value: zoom_current,
                    step: f32::MIN_POSITIVE as f64,
                    default: 1.0,
                },
                vec![],
                true,
            ));

            // zoom distortion correction
            let distortion_correction_supported =
                unsafe { device.isGeometricDistortionCorrectionSupported() };
            let distortion_correction_current_value =
                unsafe { device.isGeometricDistortionCorrectionEnabled() };

            controls.push(CameraControl::new(
                KnownCameraControl::Other(6),
                "DistortionCorrection".to_string(),
                ControlValueDescription::Boolean {
                    value: distortion_correction_current_value,
                    default: false,
                },
                if distortion_correction_supported {
                    vec![
                        KnownCameraControlFlag::ReadOnly,
                        KnownCameraControlFlag::Disabled,
                    ]
                } else {
                    vec![]
                },
                distortion_correction_supported,
            ));

            Ok(controls)
        }

        pub fn set_control(
            &mut self,
            id: KnownCameraControl,
            value: ControlValueSetter,
        ) -> Result<(), NokhwaError> {
            let rc = self.get_controls()?;
            let controls = rc
                .iter()
                .map(|cc| (cc.control(), cc))
                .collect::<BTreeMap<_, _>>();

            match id {
                KnownCameraControl::Brightness => {
                    let isoctrl = controls.get(&id).ok_or(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Control does not exist".to_string(),
                    })?;

                    if isoctrl.flag().contains(&KnownCameraControlFlag::ReadOnly) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error:
                                "Exposure is in improper state to set ISO (Please set to `custom`!)"
                                    .to_string(),
                        });
                    }

                    if isoctrl.flag().contains(&KnownCameraControlFlag::Disabled) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Disabled".to_string(),
                        });
                    }

                    let current_duration = unsafe { AVCaptureExposureDurationCurrent };
                    let new_iso = *value.as_float().ok_or(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Expected float".to_string(),
                    })? as f32;

                    if !isoctrl.description().verify_setter(&value) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Failed to verify value".to_string(),
                        });
                    }

                    let _: () = unsafe {
                        self.inner
                            .setExposureModeCustomWithDuration_ISO_completionHandler(
                                current_duration,
                                new_iso,
                                None,
                            )
                    };

                    Ok(())
                }
                KnownCameraControl::Gamma => {
                    let duration_ctrl = controls.get(&id).ok_or(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Control does not exist".to_string(),
                    })?;

                    if duration_ctrl
                        .flag()
                        .contains(&KnownCameraControlFlag::ReadOnly)
                    {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Exposure is in improper state to set Duration (Please set to `custom`!)"
                                .to_string(),
                        });
                    }

                    if duration_ctrl
                        .flag()
                        .contains(&KnownCameraControlFlag::Disabled)
                    {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Disabled".to_string(),
                        });
                    }
                    let current_duration: CMTime = unsafe { self.inner.exposureDuration() };

                    let current_iso = unsafe { AVCaptureISOCurrent };
                    let new_duration = CMTime {
                        value: *value.as_integer().ok_or(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Expected i64".to_string(),
                        })?,
                        timescale: current_duration.timescale,
                        flags: current_duration.flags,
                        epoch: current_duration.epoch,
                    };

                    if !duration_ctrl.description().verify_setter(&value) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Failed to verify value".to_string(),
                        });
                    }

                    let _: () = unsafe {
                        self.inner
                            .setExposureModeCustomWithDuration_ISO_completionHandler(
                                new_duration,
                                current_iso,
                                None,
                            )
                    };

                    Ok(())
                }
                KnownCameraControl::WhiteBalance => {
                    let wb_enum_value = controls.get(&id).ok_or(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Control does not exist".to_string(),
                    })?;

                    if wb_enum_value
                        .flag()
                        .contains(&KnownCameraControlFlag::ReadOnly)
                    {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Read Only".to_string(),
                        });
                    }

                    if wb_enum_value
                        .flag()
                        .contains(&KnownCameraControlFlag::Disabled)
                    {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Disabled".to_string(),
                        });
                    }
                    let setter = *value.as_enum().ok_or(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Expected Enum".to_string(),
                    })? as NSInteger;

                    if !wb_enum_value.description().verify_setter(&value) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Failed to verify value".to_string(),
                        });
                    }

                    unsafe {
                        self.inner
                            .setWhiteBalanceMode(av::AVCaptureWhiteBalanceMode(setter))
                    };

                    Ok(())
                }
                KnownCameraControl::BacklightComp => {
                    let ctrlvalue = controls.get(&id).ok_or(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Control does not exist".to_string(),
                    })?;

                    if ctrlvalue.flag().contains(&KnownCameraControlFlag::ReadOnly) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Read Only".to_string(),
                        });
                    }

                    if ctrlvalue.flag().contains(&KnownCameraControlFlag::Disabled) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Disabled".to_string(),
                        });
                    }

                    let setter = *value.as_enum().ok_or(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Expected Enum".to_string(),
                    })? as NSInteger;

                    if !ctrlvalue.description().verify_setter(&value) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Failed to verify value".to_string(),
                        });
                    }

                    unsafe {
                        self.inner
                            .setWhiteBalanceMode(av::AVCaptureWhiteBalanceMode(setter))
                    };

                    Ok(())
                }
                KnownCameraControl::Gain => {
                    let ctrlvalue = controls.get(&id).ok_or(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Control does not exist".to_string(),
                    })?;

                    if ctrlvalue.flag().contains(&KnownCameraControlFlag::ReadOnly) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Read Only".to_string(),
                        });
                    }

                    if ctrlvalue.flag().contains(&KnownCameraControlFlag::Disabled) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Disabled".to_string(),
                        });
                    }

                    let setter = *value.as_boolean().ok_or(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Expected Boolean".to_string(),
                    })? as NSInteger;

                    if !ctrlvalue.description().verify_setter(&value) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Failed to verify value".to_string(),
                        });
                    }

                    unsafe {
                        self.inner
                            .setWhiteBalanceMode(av::AVCaptureWhiteBalanceMode(setter))
                    };

                    Ok(())
                }
                KnownCameraControl::Zoom => {
                    let ctrlvalue = controls.get(&id).ok_or(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Control does not exist".to_string(),
                    })?;

                    if ctrlvalue.flag().contains(&KnownCameraControlFlag::ReadOnly) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Read Only".to_string(),
                        });
                    }

                    if ctrlvalue.flag().contains(&KnownCameraControlFlag::Disabled) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Disabled".to_string(),
                        });
                    }

                    let setter = *value.as_float().ok_or(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Expected float".to_string(),
                    })? as f64;

                    if !ctrlvalue.description().verify_setter(&value) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Failed to verify value".to_string(),
                        });
                    }

                    unsafe { self.inner.rampToVideoZoomFactor_withRate(setter, 1.0) };

                    Ok(())
                }
                KnownCameraControl::Exposure => {
                    let ctrlvalue = controls.get(&id).ok_or(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Control does not exist".to_string(),
                    })?;

                    if ctrlvalue.flag().contains(&KnownCameraControlFlag::ReadOnly) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Read Only".to_string(),
                        });
                    }

                    if ctrlvalue.flag().contains(&KnownCameraControlFlag::Disabled) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Disabled".to_string(),
                        });
                    }

                    let setter = *value.as_enum().ok_or(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Expected Enum".to_string(),
                    })? as NSInteger;

                    if !ctrlvalue.description().verify_setter(&value) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Failed to verify value".to_string(),
                        });
                    }

                    unsafe {
                        self.inner
                            .setExposureMode(av::AVCaptureExposureMode(setter))
                    };

                    Ok(())
                }
                KnownCameraControl::Iris => Err(NokhwaError::SetPropertyError {
                    property: id.to_string(),
                    value: value.to_string(),
                    error: "Read Only".to_string(),
                }),
                KnownCameraControl::Focus => {
                    let ctrlvalue = controls.get(&id).ok_or(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Control does not exist".to_string(),
                    })?;

                    if ctrlvalue.flag().contains(&KnownCameraControlFlag::ReadOnly) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Read Only".to_string(),
                        });
                    }

                    if ctrlvalue.flag().contains(&KnownCameraControlFlag::Disabled) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Disabled".to_string(),
                        });
                    }

                    let setter = *value.as_enum().ok_or(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Expected Enum".to_string(),
                    })? as NSInteger;

                    if !ctrlvalue.description().verify_setter(&value) {
                        return Err(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Failed to verify value".to_string(),
                        });
                    }

                    unsafe { self.inner.setFocusMode(av::AVCaptureFocusMode(setter)) };

                    Ok(())
                }
                KnownCameraControl::Other(i) => match i {
                    0 => {
                        let ctrlvalue = controls.get(&id).ok_or(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Control does not exist".to_string(),
                        })?;

                        if ctrlvalue.flag().contains(&KnownCameraControlFlag::ReadOnly) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Read Only".to_string(),
                            });
                        }

                        if ctrlvalue.flag().contains(&KnownCameraControlFlag::Disabled) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Disabled".to_string(),
                            });
                        }

                        let setter = value
                            .as_point()
                            .ok_or(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Expected Point".to_string(),
                            })
                            .map(|(x, y)| CGPoint { x: *x, y: *y })?;

                        if !ctrlvalue.description().verify_setter(&value) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Failed to verify value".to_string(),
                            });
                        }

                        unsafe { self.inner.setFocusPointOfInterest(setter) };

                        Ok(())
                    }
                    1 => {
                        let ctrlvalue = controls.get(&id).ok_or(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Control does not exist".to_string(),
                        })?;

                        if ctrlvalue.flag().contains(&KnownCameraControlFlag::ReadOnly) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Read Only".to_string(),
                            });
                        }

                        if ctrlvalue.flag().contains(&KnownCameraControlFlag::Disabled) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Disabled".to_string(),
                            });
                        }

                        let setter = *value.as_float().ok_or(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Expected float".to_string(),
                        })? as f32;

                        if !ctrlvalue.description().verify_setter(&value) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Failed to verify value".to_string(),
                            });
                        }

                        unsafe {
                            self.inner
                                .setFocusModeLockedWithLensPosition_completionHandler(setter, None)
                        };

                        Ok(())
                    }
                    2 => {
                        let ctrlvalue = controls.get(&id).ok_or(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Control does not exist".to_string(),
                        })?;

                        if ctrlvalue.flag().contains(&KnownCameraControlFlag::ReadOnly) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Read Only".to_string(),
                            });
                        }

                        if ctrlvalue.flag().contains(&KnownCameraControlFlag::Disabled) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Disabled".to_string(),
                            });
                        }

                        let setter = value
                            .as_point()
                            .ok_or(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Expected Point".to_string(),
                            })
                            .map(|(x, y)| CGPoint { x: *x, y: *y })?;

                        if !ctrlvalue.description().verify_setter(&value) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Failed to verify value".to_string(),
                            });
                        }

                        unsafe { self.inner.setExposurePointOfInterest(setter) };

                        Ok(())
                    }
                    3 => {
                        let ctrlvalue = controls.get(&id).ok_or(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Control does not exist".to_string(),
                        })?;

                        if ctrlvalue.flag().contains(&KnownCameraControlFlag::ReadOnly) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Read Only".to_string(),
                            });
                        }

                        if ctrlvalue.flag().contains(&KnownCameraControlFlag::Disabled) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Disabled".to_string(),
                            });
                        }

                        let setter = *value.as_boolean().ok_or(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Expected Boolean".to_string(),
                        })?;

                        if !ctrlvalue.description().verify_setter(&value) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Failed to verify value".to_string(),
                            });
                        }

                        unsafe {
                            self.inner
                                .setAutomaticallyAdjustsFaceDrivenAutoExposureEnabled(setter)
                        };

                        Ok(())
                    }
                    4 => {
                        let ctrlvalue = controls.get(&id).ok_or(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Control does not exist".to_string(),
                        })?;

                        if ctrlvalue.flag().contains(&KnownCameraControlFlag::ReadOnly) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Read Only".to_string(),
                            });
                        }

                        if ctrlvalue.flag().contains(&KnownCameraControlFlag::Disabled) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Disabled".to_string(),
                            });
                        }

                        let setter = *value.as_float().ok_or(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Expected Float".to_string(),
                        })? as f32;

                        if !ctrlvalue.description().verify_setter(&value) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Failed to verify value".to_string(),
                            });
                        }

                        unsafe {
                            self.inner
                                .setExposureTargetBias_completionHandler(setter, None)
                        };

                        Ok(())
                    }
                    5 => {
                        let ctrlvalue = controls.get(&id).ok_or(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Control does not exist".to_string(),
                        })?;

                        if ctrlvalue.flag().contains(&KnownCameraControlFlag::ReadOnly) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Read Only".to_string(),
                            });
                        }

                        if ctrlvalue.flag().contains(&KnownCameraControlFlag::Disabled) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Disabled".to_string(),
                            });
                        }

                        let setter = *value.as_enum().ok_or(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Expected Enum".to_string(),
                        })? as NSInteger;

                        if !ctrlvalue.description().verify_setter(&value) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Failed to verify value".to_string(),
                            });
                        }

                        unsafe { self.inner.setTorchMode(av::AVCaptureTorchMode(setter)) };

                        Ok(())
                    }
                    6 => {
                        let ctrlvalue = controls.get(&id).ok_or(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Control does not exist".to_string(),
                        })?;

                        if ctrlvalue.flag().contains(&KnownCameraControlFlag::ReadOnly) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Read Only".to_string(),
                            });
                        }

                        if ctrlvalue.flag().contains(&KnownCameraControlFlag::Disabled) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Disabled".to_string(),
                            });
                        }

                        let setter = *value.as_boolean().ok_or(NokhwaError::SetPropertyError {
                            property: id.to_string(),
                            value: value.to_string(),
                            error: "Expected Boolean".to_string(),
                        })?;

                        if !ctrlvalue.description().verify_setter(&value) {
                            return Err(NokhwaError::SetPropertyError {
                                property: id.to_string(),
                                value: value.to_string(),
                                error: "Failed to verify value".to_string(),
                            });
                        }

                        unsafe { self.inner.setGeometricDistortionCorrectionEnabled(setter) };

                        Ok(())
                    }
                    _ => Err(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: value.to_string(),
                        error: "Unknown Control".to_string(),
                    }),
                },
                _ => Err(NokhwaError::SetPropertyError {
                    property: id.to_string(),
                    value: value.to_string(),
                    error: "Unknown Control".to_string(),
                }),
            }
        }

        pub fn active_format(&self) -> Result<CameraFormat, NokhwaError> {
            let af = unsafe { self.inner.activeFormat() };
            let avf_format = AVCaptureDeviceFormat::try_from(af)?;
            let resolution = avf_format.resolution;
            let fourcc = avf_format.fourcc;
            let mut a = avf_format
                .fps_list
                .into_iter()
                .map(move |fps_f64| {
                    let fps = fps_f64 as u32;

                    let resolution =
                        Resolution::new(resolution.width as u32, resolution.height as u32); // FIXME: what the fuck?
                    CameraFormat::new(resolution, fourcc, fps)
                })
                .collect::<Vec<_>>();
            a.sort_by(|a, b| a.frame_rate().cmp(&b.frame_rate()));

            if a.len() != 0 {
                Ok(a[a.len() - 1])
            } else {
                Err(NokhwaError::GetPropertyError {
                    property: "activeFormat".to_string(),
                    error: "None??".to_string(),
                })
            }
        }
    }

    pub struct AVCaptureDeviceInput {
        inner: Retained<av::AVCaptureDeviceInput>,
    }

    impl AVCaptureDeviceInput {
        pub fn inner(&self) -> &Retained<av::AVCaptureDeviceInput> {
            &self.inner
        }

        pub fn new(capture_device: &AVCaptureDevice) -> Result<Self, NokhwaError> {
            let inner = unsafe {
                av::AVCaptureDeviceInput::deviceInputWithDevice_error(capture_device.inner())
            }
            .map_err(|err| NokhwaError::InitializeError {
                backend: ApiBackend::AVFoundation,
                error: format!("Failed to create input: {}", err.localizedDescription()),
            })?;

            Ok(AVCaptureDeviceInput { inner })
        }
    }

    pub struct AVCaptureVideoDataOutput {
        inner: Retained<av::AVCaptureVideoDataOutput>,
    }

    impl AVCaptureVideoDataOutput {
        pub fn inner(&self) -> &Retained<av::AVCaptureVideoDataOutput> {
            &self.inner
        }

        pub fn new() -> Self {
            AVCaptureVideoDataOutput::default()
        }

        pub fn add_delegate(&self, delegate: &AVCaptureVideoCallback) -> Result<(), NokhwaError> {
            unsafe {
                self.inner.setSampleBufferDelegate_queue(
                    Some(delegate.delegate()),
                    Some(delegate.queue()),
                );
            };
            Ok(())
        }

        pub fn set_frame_format(&self, format: FrameFormat) -> Result<(), NokhwaError> {
            let cmpixelfmt = match format {
                FrameFormat::YUYV => kCMPixelFormat_422YpCbCr8_yuvs,
                FrameFormat::MJPEG => kCMVideoCodecType_JPEG,
                FrameFormat::GRAY => kCMPixelFormat_8IndexedGray_WhiteIsZero,
                FrameFormat::NV12 => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                FrameFormat::RAWRGB => kCMPixelFormat_24RGB,
                FrameFormat::RAWBGR => {
                    return Err(NokhwaError::SetPropertyError {
                        property: "setVideoSettings".to_string(),
                        value: "set frame format".to_string(),
                        error: "Unsupported frame format BGR".to_string(),
                    });
                }
            };
            let value = NSNumber::new_i32(cmpixelfmt as i32);
            // SAFETY: CFString is toll-free bridged to NSString.
            let key: &NSString =
                unsafe { &*(kCVPixelBufferPixelFormatTypeKey as *const _ as *const NSString) };
            // SAFETY: The upcast is sound, as NSNumber is an object.
            let value = unsafe { Retained::cast_unchecked::<AnyObject>(value) };
            let settings = NSDictionary::from_retained_objects(&[key], &[value]);
            // SAFETY: The values are NSNumbers, which is what AVFoundation expects here.
            unsafe { self.inner.setVideoSettings(Some(&settings)) };
            Ok(())
        }
    }

    impl Default for AVCaptureVideoDataOutput {
        fn default() -> Self {
            AVCaptureVideoDataOutput {
                inner: unsafe { av::AVCaptureVideoDataOutput::new() },
            }
        }
    }

    pub struct AVCaptureSession {
        inner: Retained<av::AVCaptureSession>,
    }

    impl AVCaptureSession {
        pub fn inner(&self) -> &Retained<av::AVCaptureSession> {
            &self.inner
        }

        pub fn new() -> Self {
            AVCaptureSession::default()
        }

        pub fn begin_configuration(&self) {
            unsafe { self.inner.beginConfiguration() }
        }

        pub fn commit_configuration(&self) {
            unsafe { self.inner.commitConfiguration() }
        }

        pub fn can_add_input(&self, input: &AVCaptureDeviceInput) -> bool {
            unsafe { self.inner.canAddInput(&input.inner) }
        }

        pub fn add_input(&self, input: &AVCaptureDeviceInput) -> Result<(), NokhwaError> {
            if self.can_add_input(input) {
                unsafe { self.inner.addInput(&input.inner) };
                return Ok(());
            }
            Err(NokhwaError::SetPropertyError {
                property: "AVCaptureDeviceInput".to_string(),
                value: "add new input".to_string(),
                error: "Rejected".to_string(),
            })
        }

        pub fn remove_input(&self, input: &AVCaptureDeviceInput) {
            unsafe { self.inner.removeInput(&input.inner) }
        }

        pub fn can_add_output(&self, output: &AVCaptureVideoDataOutput) -> bool {
            unsafe { self.inner.canAddOutput(&output.inner) }
        }

        pub fn add_output(&self, output: &AVCaptureVideoDataOutput) -> Result<(), NokhwaError> {
            if self.can_add_output(output) {
                unsafe { self.inner.addOutput(&output.inner) };
                return Ok(());
            }
            Err(NokhwaError::SetPropertyError {
                property: "AVCaptureVideoDataOutput".to_string(),
                value: "add new output".to_string(),
                error: "Rejected".to_string(),
            })
        }

        pub fn remove_output(&self, output: &AVCaptureVideoDataOutput) {
            unsafe { self.inner.removeOutput(&output.inner) }
        }

        pub fn is_running(&self) -> bool {
            unsafe { self.inner.isRunning() }
        }

        pub fn start(&self) -> Result<(), NokhwaError> {
            let start_stream_fn = || {
                unsafe { self.inner.startRunning() };
            };

            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(start_stream_fn)).is_err() {
                return Err(NokhwaError::OpenStreamError(
                    "Cannot run AVCaptureSession".to_string(),
                ));
            }
            Ok(())
        }

        pub fn stop(&self) {
            unsafe { self.inner.stopRunning() }
        }

        pub fn is_interrupted(&self) -> bool {
            unsafe { self.inner.isInterrupted() }
        }
    }

    impl Default for AVCaptureSession {
        fn default() -> Self {
            AVCaptureSession {
                inner: unsafe { av::AVCaptureSession::new() },
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use crate::internal::*;
