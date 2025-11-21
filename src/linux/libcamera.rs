use std::{sync::Arc, thread, time::{SystemTime, UNIX_EPOCH}};
use std::num::NonZeroU32;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;
use libcamera::{
    camera::ActiveCamera,
    camera_manager::CameraManager,
    framebuffer_allocator::{FrameBufferAllocator},
    request::ReuseFlag,
    stream::StreamRole,
};
use libcamera::camera::CameraConfiguration;
use libcamera::framebuffer::AsFrameBuffer;
use libcamera::framebuffer_allocator::FrameBuffer;
use libcamera::framebuffer_map::MemoryMappedFrameBuffer;
use media::media_frame::MediaFrame;
use media::video::{CompressionFormat, PixelFormat, VideoFormat, VideoFrameDescription};
use crate::{
    device::{Device, DeviceEvent, OutputDevice, DeviceManager},
    error::DeviceError,
    variant::Variant,
};

const PIXEL_FORMAT_MJPEG: libcamera::pixel_format::PixelFormat = libcamera::pixel_format::PixelFormat::new(FOURCC_MJPEG, 0);
const PIXEL_FORMAT_NV12: libcamera::pixel_format::PixelFormat = libcamera::pixel_format::PixelFormat::new(FOURCC_NV12, 0);
const PIXEL_FORMAT_YUYV: libcamera::pixel_format::PixelFormat = libcamera::pixel_format::PixelFormat::new(FOURCC_YUYV, 0);
const FOURCC_MJPEG: u32 = u32::from_le_bytes([b'M', b'J', b'P', b'G']);
const FOURCC_NV12: u32 = u32::from_le_bytes([b'N', b'V', b'1', b'2']);
const FOURCC_YUYV: u32 = u32::from_le_bytes([b'Y', b'U', b'Y', b'V']);

type OutputHanderFn = dyn Fn(MediaFrame) -> Result<(), DeviceError> + Send + Sync;
type OutputHandlerArc = Arc<OutputHanderFn>;

struct LinuxCameraWorker {
    camera: ActiveCamera<'static>,
    alloc: FrameBufferAllocator,
    output_handler: Option<OutputHandlerArc>,
    config: CameraConfiguration,
    cmd_rx: mpsc::Receiver<CameraCmd>,
    cmd_response_tx: mpsc::Sender<CameraCmdResponse>,
}

// Safety: the `ActiveCamera` is only used by the worker thread
unsafe impl Send for LinuxCameraWorker {}

impl LinuxCameraWorker {
    fn run(mut instance: LinuxCameraWorker) {

        let mut req_rx = None;
        let mut running = false;
        let mut shutdown = false;

        while !shutdown {
            // process all outstanding commands
            while let Ok(cmd) = instance.cmd_rx.try_recv() {
                if shutdown {
                    break;
                }

                match cmd {
                    CameraCmd::Start => {
                        if running {
                            let _ = instance.cmd_response_tx.send(CameraCmdResponse::DeviceError(DeviceError::StartFailed("Already running".into())));
                            continue
                        }

                        let handler = instance.output_handler.clone();
                        let stream_cfg = instance.config
                            .get_mut(0).unwrap();

                        let stream = stream_cfg.stream().unwrap();

                        let size = stream_cfg.get_size();
                        let format: libcamera::pixel_format::PixelFormat = stream_cfg.get_pixel_format();
                        let pixel_format = match format.fourcc() {
                            FOURCC_NV12 => crate::media::video::PixelFormat::NV12,
                            FOURCC_YUYV => crate::media::video::PixelFormat::YUYV,
                            _ => {
                                let _ = instance.cmd_response_tx.send(CameraCmdResponse::DeviceError(DeviceError::StartFailed(format!("Unsupported pixel format. {:?}", format).into())));
                                continue;
                            },
                        };

                        let desc = VideoFrameDescription::new(
                            pixel_format,
                            unsafe { NonZeroU32::new_unchecked(size.width) },
                            unsafe { NonZeroU32::new_unchecked(size.height) },
                        );

                        let (req_tx, new_req_rx) = mpsc::channel::<libcamera::request::Request>();
                        req_rx = Some(new_req_rx);

                        if let Some(handler) = handler {
                            // Set callback for completed requests
                            instance.camera.on_request_completed({
                                let desc = desc.clone();
                                move |req| {
                                    if let Some(framebuffer) = req.buffer::<MemoryMappedFrameBuffer<FrameBuffer>>(&stream) {
                                        if let Some(plane) = framebuffer.data().get(0) {
                                            let bytes_used = framebuffer.planes().get(0).unwrap().len() as usize;
                                            let data = plane[..bytes_used].to_vec();

                                            let timestamp = SystemTime::now()
                                                .duration_since(UNIX_EPOCH)
                                                .unwrap()
                                                .as_micros() as u64;

                                            let mut frame = MediaFrame::from_data_buffer(desc.clone(), data.as_slice()).unwrap();
                                            frame.timestamp = timestamp;

                                            let _ = handler(frame);
                                        }
                                    }

                                    // Reuse and requeue
                                    req_tx.send(req).unwrap();
                                }
                            });
                        }

                        let buffers = instance.alloc
                            .alloc(&stream)
                            .unwrap()
                            .into_iter()
                            .map(|b| MemoryMappedFrameBuffer::new(b).unwrap())
                            .collect::<Vec<_>>();

                        let reqs = buffers
                            .into_iter()
                            .enumerate()
                            .map(|(i, buf)| {
                                let mut req = instance.camera.create_request(Some(i as u64)).unwrap();
                                req.add_buffer(&stream, buf).unwrap();
                                req
                            })
                            .collect::<Vec<_>>();

                        if let Err(e) = instance.camera.start(None) {
                            let _ = instance.cmd_response_tx.send(CameraCmdResponse::DeviceError(DeviceError::StartFailed(format!("{e:?}"))));
                            continue;
                        };

                        // Enqueue all requests to the camera
                        for req in reqs {
                            println!("Request queued for execution: {req:#?}");
                            instance.camera.queue_request(req).unwrap();
                        }

                        let _ = instance.cmd_response_tx.send(CameraCmdResponse::Ok);
                        running = true;
                    }
                    CameraCmd::Stop => {
                        if !running {
                            let _ = instance.cmd_response_tx.send(CameraCmdResponse::DeviceError(DeviceError::NotRunning("Not running".to_string())));
                            continue;
                        }

                        if let Err(e) = instance.camera.stop() {
                            let _ = instance.cmd_response_tx.send(CameraCmdResponse::DeviceError(DeviceError::StopFailed(format!("{e:?}"))));
                        }
                        let _ = instance.cmd_response_tx.send(CameraCmdResponse::Ok);
                        running = false;
                    }
                    CameraCmd::Shutdown => {
                        shutdown = true;
                        break
                    }
                    CameraCmd::SetOutputHandler(handler) => {
                        instance.output_handler = Some(handler);
                        let _ = instance.cmd_response_tx.send(CameraCmdResponse::Ok);
                    }
                    CameraCmd::Configure(options) => {
                        let mut stream_config = instance.config
                            .get_mut(0).unwrap();

                        // TODO match the options against a valid format for this device, since the supplied values may be wrong or result in an invalid combination.
                        let desired_size = if let (Some(width), Some(height)) = (options["width"].get_uint32(), options["height"].get_uint32()) {
                            Some(libcamera::geometry::Size { width, height })
                        } else {
                            None
                        };

                        if let Some(desired_size) = desired_size {
                            println!("desired size: {:?}", desired_size);
                            stream_config.set_size(desired_size);
                        }

                        let video_format = options["format"].get_uint32();

                        let video_format = match video_format {
                            Some(video_format) => media::video::VideoFormat::try_from(video_format).ok(),
                            None => None,
                        };

                        println!("video format: {:?}", video_format);

                        match video_format {
                            Some(VideoFormat::Pixel(PixelFormat::NV12)) => stream_config.set_pixel_format(PIXEL_FORMAT_NV12),
                            Some(VideoFormat::Pixel(PixelFormat::YUYV)) => stream_config.set_pixel_format(PIXEL_FORMAT_YUYV),
                            Some(VideoFormat::Compression(CompressionFormat::MJPEG)) => stream_config.set_pixel_format(PIXEL_FORMAT_MJPEG),
                            Some(_) => {
                                // TODO: handle other formats
                                unimplemented!()
                            },
                            None => {
                                // XXX temporarily use YUYV as default format
                                stream_config.set_pixel_format(PIXEL_FORMAT_YUYV)
                            }
                        };

                        let frame_rate = options["frame-rate"].get_float();
                        if let Some(frame_rate) = frame_rate {
                            // TODO
                        }

                        // drop the reference to avoid borrow checker issues
                        drop(stream_config);

                        // avoid borrow checker issues by taking the config
                        let configuration_result = {
                            instance.config.validate();
                            let result = instance.camera.configure(&mut instance.config);

                            // XXX
                            println!("config: {:?}", instance.config);
                            result
                        };
                        if let Err(e) = configuration_result {
                            if instance.cmd_response_tx.send(CameraCmdResponse::DeviceError(DeviceError::SetFailed(format!("{e:?}")))).is_err() {
                                shutdown = true;
                            }
                            break
                        }


                        if let Some(desired_size) = desired_size {
                            let stream_config = instance.config
                                .get_mut(0).unwrap();
                            let actual_size = stream_config.get_size();
                            assert_eq!((desired_size.width, desired_size.height), (actual_size.width, actual_size.height));
                        }
                        if instance.cmd_response_tx.send(CameraCmdResponse::Ok).is_err() {
                            break
                        }
                    }
                }
            }
            if let Some(req_rx) = req_rx.as_mut() {
                if let Ok(mut req) = req_rx.recv_timeout(Duration::from_millis(250)) {
                    if running {
                        req.reuse(ReuseFlag::REUSE_BUFFERS);
                        if let Err(e) = instance.camera.queue_request(req) {
                            eprintln!("queue_request failed: {:?}", e);
                            break;
                        }
                    } else {
                        drop(req);
                    }
                }
            }
        }
    }
}

enum CameraCmd
{
    Start,
    Stop,
    Shutdown,
    SetOutputHandler(OutputHandlerArc),
    Configure(Variant),
}

enum CameraCmdResponse {
    Ok,
    DeviceError(DeviceError),
}

struct LinuxCameraWorkerHandle {
    join: JoinHandle<()>,
}

/// Linux backend device
pub struct LinuxCameraDevice {
    id: String,
    running: bool,
    worker_handle: Option<LinuxCameraWorkerHandle>,
    cmd_tx: mpsc::Sender<CameraCmd>,
    cmd_response_rx: mpsc::Receiver<CameraCmdResponse>,
}

impl LinuxCameraDevice {
    pub fn new(
        camera: ActiveCamera<'static>
    ) -> Self {
        let id = camera.id().to_string();

        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<CameraCmd>();
        let (cmd_response_tx, cmd_response_rx) = std::sync::mpsc::channel::<CameraCmdResponse>();

        let config = camera.generate_configuration(&[StreamRole::VideoRecording]).unwrap();
        let alloc = FrameBufferAllocator::new(&camera);

        let worker = LinuxCameraWorker {
            camera,
            config,
            alloc,
            output_handler: None,
            cmd_rx,
            cmd_response_tx,
        };

        let worker_join_handle = thread::spawn(move || { LinuxCameraWorker::run(worker)});

        Self {
            id,
            running: false,
            worker_handle: Some(LinuxCameraWorkerHandle { join: worker_join_handle }),
            cmd_tx,
            cmd_response_rx,
        }
    }
}

impl Device for LinuxCameraDevice {
    fn name(&self) -> &str {
        //self.camera.properties().get::<Model>().unwrap()
        "TODO"
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn start(&mut self) -> Result<(), DeviceError> {
        self.cmd_tx.send(CameraCmd::Start)
            .map_err(|e| DeviceError::StartFailed(format!("Failed to send start command: {:?}", e)))?;
        match self.cmd_response_rx.recv()
            .map_err(|e| DeviceError::StartFailed(format!("No response to command: {:?}", e)))?
        {
            CameraCmdResponse::Ok => Ok(()),
            CameraCmdResponse::DeviceError(e) => Err(e),
        }
    }

    fn stop(&mut self) -> Result<(), DeviceError> {
        self.cmd_tx.send(CameraCmd::Stop)
            .map_err(|e| DeviceError::CloseFailed(format!("Failed to send close command: {:?}", e)))?;
        match self.cmd_response_rx.recv()
            .map_err(|e| DeviceError::StartFailed(format!("No response to command: {:?}", e)))?
        {
            CameraCmdResponse::Ok => Ok(()),
            CameraCmdResponse::DeviceError(e) => Err(e),
        }
    }

    fn configure(&mut self, options: Variant) -> Result<(), DeviceError> {
        self.cmd_tx.send(CameraCmd::Configure(options))
            .map_err(|e| DeviceError::SetFailed(format!("Failed to send configure command: {:?}", e)))?;
        match self.cmd_response_rx.recv()
            .map_err(|e| DeviceError::SetFailed(format!("No response to command: {:?}", e)))?
        {
            CameraCmdResponse::Ok => Ok(()),
            CameraCmdResponse::DeviceError(e) => Err(e),
        }
    }

    fn control(&mut self, _action: Variant) -> Result<(), DeviceError> {
        // Not implemented yet
        Ok(())
    }

    fn running(&self) -> bool {
        self.running
    }

    fn formats(&self) -> Result<Variant, DeviceError> {
        // Not implemented yet
        Ok(Variant::None)
    }
}

impl Drop for LinuxCameraDevice {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(CameraCmd::Shutdown);

        let handle = self.worker_handle.take().unwrap();

        let _ = handle.join.join();
    }
}

impl<'a> OutputDevice for LinuxCameraDevice {
    fn set_output_handler<F>(&mut self, handler: F) -> Result<(), DeviceError>
    where
        F: Fn(MediaFrame) -> Result<(), DeviceError> + Send + Sync + 'static,
    {
        self.cmd_tx.send(CameraCmd::SetOutputHandler(Arc::new(handler)))
            .map_err(|e| DeviceError::SetFailed(format!("Failed to send set output handler command: {:?}", e)))?;
        match self.cmd_response_rx.recv()
            .map_err(|e| DeviceError::SetFailed(format!("No response to command: {:?}", e)))?
        {
            CameraCmdResponse::Ok => Ok(()),
            CameraCmdResponse::DeviceError(e) => Err(e),
        }
    }
}

/// Linux backend device manager
pub struct LinuxCameraManager {
    mgr: CameraManager,
    devices: Vec<LinuxCameraDevice>,
    change_handler: Option<Arc<dyn Fn(&DeviceEvent) + Send + Sync>>,
}

impl DeviceManager for LinuxCameraManager {
    type DeviceType = LinuxCameraDevice;

    fn init() -> Result<Self, DeviceError> {
        let mgr = CameraManager::new()
            .map_err(|e| DeviceError::OpenFailed(format!("{e:?}")))?;

        let devices = Vec::new();

        Ok(Self {
            mgr,
            devices,
            change_handler: None,
        })
    }

    fn uninit(&mut self) {
        self.devices.clear();
    }

    fn list(&self) -> Vec<&Self::DeviceType> {
        self.devices.iter().collect()
    }

    fn index(&self, index: usize) -> Option<&Self::DeviceType> {
        self.devices.get(index)
    }

    fn index_mut(&mut self, index: usize) -> Option<&mut Self::DeviceType> {
        self.devices.get_mut(index)
    }

    fn lookup(&self, id: &str) -> Option<&Self::DeviceType> {
        self.devices.iter().find(|d| d.id() == id)
    }

    fn lookup_mut(&mut self, id: &str) -> Option<&mut Self::DeviceType> {
        self.devices.iter_mut().find(|d| d.id() == id)
    }

    fn refresh(&mut self) -> Result<(), DeviceError> {

        self.devices.clear();

        let cameras = self.mgr.cameras();
        for i in 0..cameras.len() {
            if let Some(cam) = cameras.get(i) {
                let cam = cam.acquire().map_err(|e| DeviceError::OpenFailed(format!("{e:?}")))?;
                let cam: ActiveCamera<'static> = unsafe { std::mem::transmute(cam) };

                let dev = LinuxCameraDevice::new(cam);
                self.devices.push(dev);
            }
        }

        Ok(())
    }

    fn set_change_handler<F>(&mut self, handler: F) -> Result<(), DeviceError>
    where
        F: Fn(&DeviceEvent) + Send + Sync + 'static,
    {
        self.change_handler = Some(Arc::new(handler));
        Ok(())
    }
}


/*
use std::{
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}},
    thread,
};
use libcamera::camera::{ActiveCamera, Camera};
use libcamera::camera_manager::CameraManager;
use libcamera::stream::StreamRole;
use crate::device::{Device, OutputDevice, DeviceManager, DeviceEvent, DeviceInformation};
use crate::error::DeviceError;
use x_media::media_frame::MediaFrame;
use x_variant::Variant;

/// A single libcamera device
pub struct LibcameraDevice {
    id: String,
    name: String,
    camera: Arc<Camera<'static>>,
    active: Option<ActiveCamera<'static>>,
    running: Arc<AtomicBool>,
    capture_thread: Option<thread::JoinHandle<()>>,
    frame_handler: Arc<Mutex<Option<Box<dyn Fn(MediaFrame) -> Result<(), DeviceError> + Send + Sync>>>>,
}

impl LibcameraDevice {
    pub fn new(camera: Arc<Camera<'static>>) -> Result<Self, DeviceError> {
        let id = camera.id().to_string();
        let name = "TODO".to_string();

        Ok(Self {
            id,
            name,
            camera,
            active: None,
            running: Arc::new(AtomicBool::new(false)),
            capture_thread: None,
            frame_handler: Arc::new(Mutex::new(None)),
        })
    }

    /// Acquire the ActiveCamera
    fn acquire_camera(&mut self) -> Result<(), DeviceError> {
        let active = unsafe { std::mem::transmute::<ActiveCamera<'_>, ActiveCamera<'static>>(self.camera.acquire()
            .map_err(|e| DeviceError::OpenFailed(format!("acquire failed: {:?}", e)))?) };
        self.active = Some(active);
        Ok(())
    }
}

impl Device for LibcameraDevice {
    fn name(&self) -> &str { &self.name }
    fn id(&self) -> &str { &self.id }

    fn start(&mut self) -> Result<(), DeviceError> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }

        self.acquire_camera()?;

        let active = self.active.as_mut().ok_or(DeviceError::StartFailed("ActiveCamera missing".into()))?;

        // Generate configuration
        let mut config = self.camera.generate_configuration(&[StreamRole::VideoRecording])
            .ok_or(DeviceError::StartFailed("generate_configuration failed".into()))?;
        config.validate();
        active.configure(&mut config)
            .map_err(|e| DeviceError::SetFailed(format!("configure failed: {:?}", e)))?;

        active.start(None)
            .map_err(|e| DeviceError::StartFailed(format!("start capture failed: {:?}", e)))?;

        let running = self.running.clone();
        let handler = self.frame_handler.clone();
        let mut active_clone = unsafe { std::mem::transmute::<&mut ActiveCamera<'_>, &mut ActiveCamera<'static>>(active) };

        self.running.store(true, Ordering::SeqCst);

        self.capture_thread = Some(thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                if let Some(mut req) = active_clone.create_request(None) {
                    if req.attach_buffers().is_err() {
                        eprintln!("Failed to attach buffers");
                        break;
                    }

                    // Setup callback to capture completed frames
                    req.queue(&mut active_clone).unwrap_or_else(|e| {
                        eprintln!("Queue request failed: {:?}", e);
                    });

                    // RequestCompleted callback gives us the frame
                    active_clone.on_request_completed(move |req| {
                        for (_, buf) in req.buffers() {
                            if let Some(cb) = &*handler.lock().unwrap() {
                                if let Some(plane) = buf.planes().get(0) {
                                    let data = unsafe {
                                        std::slice::from_raw_parts(plane.mem_ptr() as *const u8, plane.len())
                                    };
                                    MediaFrame::from_data_buffer()
                                    let frame = MediaFrame {
                                        data: data.to_vec(),
                                        width: buf.width() as u32,
                                        height: buf.height() as u32,
                                        timestamp: buf.metadata().timestamp,
                                    };
                                    let _ = cb(frame);
                                }
                            }
                        }
                    });
                }
            }

            active_clone.stop().unwrap_or_else(|e| {
                eprintln!("Stop capture failed: {:?}", e);
            });
        }));

        Ok(())
    }

    fn stop(&mut self) -> Result<(), DeviceError> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.capture_thread.take() {
            let _ = handle.join();
        }

        if let Some(active) = self.active.take() {
            active.stop().map_err(|e| DeviceError::StopFailed(format!("{:?}", e)))?;
        }

        Ok(())
    }

    fn configure(&mut self, _options: Variant) -> Result<(), DeviceError> {
        Ok(())
    }

    fn control(&mut self, _action: Variant) -> Result<(), DeviceError> {
        Ok(())
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn formats(&self) -> Result<Variant, DeviceError> {
        Ok(Variant::None)
    }
}

impl OutputDevice for LibcameraDevice {
    fn set_output_handler<F>(&mut self, handler: F) -> Result<(), DeviceError>
    where
        F: Fn(MediaFrame) -> Result<(), DeviceError> + Send + Sync + 'static
    {
        *self.frame_handler.lock().unwrap() = Some(Box::new(handler));
        Ok(())
    }
}

/// DeviceManager for libcamera
pub struct LibcameraDeviceManager {
    devices: Vec<LibcameraDevice>,
}

impl DeviceManager for LibcameraDeviceManager {
    type DeviceType = LibcameraDevice;

    fn init() -> Result<Self, DeviceError> {
        let cm = CameraManager::new()
            .map_err(|e| DeviceError::OpenFailed(format!("CameraManager init failed: {:?}", e)))?;

        let mut devices = Vec::new();
        for cam in cm.cameras() {
            let arc_cam = cam.clone();
            devices.push(LibcameraDevice::new(arc_cam)?);
        }

        Ok(Self { devices })
    }

    fn uninit(&mut self) {}

    fn list(&self) -> Vec<&Self::DeviceType> {
        self.devices.iter().collect()
    }

    fn index(&self, index: usize) -> Option<&Self::DeviceType> {
        self.devices.get(index)
    }

    fn index_mut(&mut self, index: usize) -> Option<&mut Self::DeviceType> {
        self.devices.get_mut(index)
    }

    fn lookup(&self, id: &str) -> Option<&Self::DeviceType> {
        self.devices.iter().find(|d| d.id() == id)
    }

    fn lookup_mut(&mut self, id: &str) -> Option<&mut Self::DeviceType> {
        self.devices.iter_mut().find(|d| d.id() == id)
    }

    fn refresh(&mut self) -> Result<(), DeviceError> {
        Ok(())
    }

    fn set_change_handler<F>(&mut self, _handler: F) -> Result<(), DeviceError>
    where
        F: Fn(&DeviceEvent) + Send + Sync + 'static
    {
        Ok(())
    }
}
*/