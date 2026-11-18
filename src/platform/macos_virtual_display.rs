//! Owns a software display in the current macOS console session.
use std::{
    collections::BTreeMap,
    sync::{Mutex, mpsc},
    thread,
};

use display_control::{DisplayCapabilities, DisplayController, DisplayMode};
use objc2::{
    encode::{Encode, Encoding},
    msg_send,
    rc::{Retained, autoreleasepool},
    runtime::{AnyClass, AnyObject, Bool},
};

use super::*;
use crate::{
    audio::UnavailableAudio,
    platform::{SessionLifecycle, native::NativeBackend},
};

static OWNED_DISPLAYS: Mutex<BTreeMap<u32, DisplayMode>> = Mutex::new(BTreeMap::new());
const MAX_WIDTH: u32 = 3840;
const MAX_HEIGHT: u32 = 2160;
const INITIAL_MODE: DisplayMode = DisplayMode {
    width: 1920,
    height: 1080,
    scale: 1.0,
};
const SCALES: &[f64] = &[1.0, 2.0];

enum DisplayCommand {
    Resize(DisplayMode, mpsc::SyncSender<Result<()>>),
    Stop,
}

pub(super) fn is_extended(id: u32) -> bool {
    unsafe { CGDisplayIsInMirrorSet(id) == 0 }
}

pub(super) fn owns(id: u32) -> bool {
    mode(id).is_some()
}

pub(super) fn mode(id: u32) -> Option<DisplayMode> {
    OWNED_DISPLAYS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&id)
        .copied()
}

pub(super) fn check_available(platform: &MacosPlatform) -> Result<()> {
    ensure!(
        platform.session_active()?,
        "虚拟显示器需要已解锁的 macOS 登录会话"
    );
    platform.permissions()?.require_granted()?;
    Classes::load()?;
    Ok(())
}

pub(super) fn open(config: &Config, cancelled: Arc<AtomicBool>) -> Result<HostSession> {
    let platform = MacosPlatform::default();
    check_available(&platform)?;
    let key = platform.session_key()?;
    let session = VirtualDisplay::start(key.clone(), cancelled.as_ref())?;
    let output = session.wait_for_output(cancelled.as_ref())?;
    let platform = Arc::new(MacosPlatform {
        pointer: Some(Mutex::new(macos_pointer::VirtualPointer::new(session.id))),
    });
    let backend = Arc::new(NativeBackend::start(
        platform,
        key,
        output.clone(),
        config.fps,
        cancelled,
    )?);
    Ok(HostSession {
        displays: Arc::new(VirtualOutput {
            id: session.id,
            commands: session.commands.clone(),
            backend: backend.clone(),
        }),
        backend,
        lifecycle: Box::new(session),
        audio: Arc::new(UnavailableAudio),
        output: Some(output.name.clone()),
        outputs: vec![output],
    })
}

struct VirtualDisplay {
    id: u32,
    key: String,
    commands: mpsc::Sender<DisplayCommand>,
    worker: Option<thread::JoinHandle<()>>,
}

impl VirtualDisplay {
    fn start(key: String, cancelled: &AtomicBool) -> Result<Self> {
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "virtual display startup cancelled"
        );
        let (ready, started) = mpsc::sync_channel(1);
        let (commands, requests) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("macos-virtual-display".into())
            .spawn(move || match autoreleasepool(|_| OwnedDisplay::create()) {
                Ok(mut display) => {
                    if ready.send(Ok(display.id)).is_ok() {
                        while let Ok(DisplayCommand::Resize(mode, reply)) = requests.recv() {
                            let result = autoreleasepool(|_| display.resize(mode));
                            let _ = reply.send(result);
                        }
                    }
                    autoreleasepool(|_| drop(display));
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            })?;
        let deadline = Instant::now() + Duration::from_secs(4);
        let id = loop {
            ensure!(
                !cancelled.load(Ordering::Acquire),
                "virtual display startup cancelled"
            );
            ensure!(
                Instant::now() < deadline,
                "macOS virtual display creation timed out"
            );
            match started.recv_timeout(Duration::from_millis(25)) {
                Ok(result) => break result?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("macOS virtual display worker stopped")
                }
            }
        };
        Ok(Self {
            id,
            key,
            commands,
            worker: Some(worker),
        })
    }

    fn wait_for_output(&self, cancelled: &AtomicBool) -> Result<DisplayOutput> {
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            ensure!(
                !cancelled.load(Ordering::Acquire),
                "virtual display startup cancelled"
            );
            if unsafe { CGDisplayIsActive(self.id) } != 0
                && let Some(display) = SCShareableContent::get()?
                    .displays()
                    .into_iter()
                    .find(|display| display.display_id() == self.id)
            {
                ensure!(
                    is_extended(self.id),
                    "请在 macOS 显示器设置中将虚拟显示器设为扩展显示器"
                );
                return display_output(&display);
            }
            thread::sleep(Duration::from_millis(80));
        }
        bail!("macOS 未将虚拟显示器登记为可采集的显示器")
    }
}

impl SessionLifecycle for VirtualDisplay {
    fn healthy(&mut self) -> bool {
        (unsafe { CGDisplayIsActive(self.id) != 0 })
            && MacosPlatform::default()
                .session_key()
                .is_ok_and(|key| key == self.key)
    }

    fn is_owned(&self) -> bool {
        true
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        let _ = self.commands.send(DisplayCommand::Stop);
        if let Some(worker) = self.worker.take() {
            crate::platform::native::join_until(worker, Instant::now() + Duration::from_secs(1));
        }
    }
}

struct VirtualOutput {
    id: u32,
    commands: mpsc::Sender<DisplayCommand>,
    backend: Arc<NativeBackend>,
}

impl DisplayController for VirtualOutput {
    fn outputs(&self) -> Result<Vec<DisplayOutput>> {
        ensure!(
            unsafe { CGDisplayIsActive(self.id) } != 0,
            "macOS virtual display disconnected"
        );
        Ok(vec![display_geometry(self.id, unsafe {
            CGDisplayBounds(self.id)
        })?])
    }

    fn capabilities(&self) -> BoxFuture<'_, DisplayCapabilities> {
        Box::pin(async {
            DisplayCapabilities {
                resize: true,
                resize_scale: true,
                reason: None,
            }
        })
    }

    fn resize_scales(&self) -> &[f64] {
        SCALES
    }

    fn resize<'a>(
        &'a self,
        output: Option<&'a str>,
        mode: DisplayMode,
        _: u32,
    ) -> BoxFuture<'a, Result<DisplayMode>> {
        Box::pin(async move {
            mode.validate()?;
            ensure!(
                SCALES.contains(&mode.scale),
                "macOS virtual display supports 1x and 2x scaling"
            );
            ensure!(
                output.is_none_or(|name| name == format!("display-{}", self.id)),
                "selected virtual display unavailable"
            );
            let commands = self.commands.clone();
            self.backend
                .reconfigure_display(move || {
                    let (reply, complete) = mpsc::sync_channel(1);
                    commands
                        .send(DisplayCommand::Resize(mode, reply))
                        .map_err(|_| anyhow::anyhow!("macOS virtual display worker stopped"))?;
                    complete
                        .recv_timeout(Duration::from_secs(5))
                        .context("macOS virtual display mode change did not complete")?
                })
                .await?;
            Ok(mode)
        })
    }
}

struct Classes {
    descriptor: &'static AnyClass,
    display: &'static AnyClass,
    settings: &'static AnyClass,
    mode: &'static AnyClass,
}

impl Classes {
    fn load() -> Result<Self> {
        let load = |name| AnyClass::get(name).context("当前 macOS 不提供虚拟显示器接口");
        Ok(Self {
            descriptor: load(c"CGVirtualDisplayDescriptor")?,
            display: load(c"CGVirtualDisplay")?,
            settings: load(c"CGVirtualDisplaySettings")?,
            mode: load(c"CGVirtualDisplayMode")?,
        })
    }
}

struct OwnedDisplay {
    id: u32,
    object: Option<Retained<AnyObject>>,
    mode: DisplayMode,
}

#[repr(C)]
struct SizeInMillimeters {
    width: f64,
    height: f64,
}

// Both supported macOS targets define CGFloat as double.
unsafe impl Encode for SizeInMillimeters {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

impl OwnedDisplay {
    fn create() -> Result<Self> {
        let classes = Classes::load()?;
        // These private CoreGraphics classes are resolved at runtime. Use the
        // same descriptor, display and settings API as DeskPad.
        unsafe {
            let descriptor: Option<Retained<AnyObject>> = msg_send![classes.descriptor, new];
            let descriptor = descriptor.context("create macOS virtual display descriptor")?;
            let name: Option<Retained<AnyObject>> = msg_send![
                AnyClass::get(c"NSString").context("NSString unavailable")?,
                stringWithUTF8String: c"LightOS Virtual Display".as_ptr()
            ];
            let name = name.context("create macOS virtual display name")?;
            let queue = dispatch_get_global_queue(0, 0)
                .as_ref()
                .context("get macOS virtual display queue")?;
            let _: () = msg_send![&*descriptor, setDispatchQueue: queue];
            let _: () = msg_send![&*descriptor, setName: &*name];
            let _: () = msg_send![&*descriptor, setMaxPixelsWide: MAX_WIDTH];
            let _: () = msg_send![&*descriptor, setMaxPixelsHigh: MAX_HEIGHT];
            let _: () = msg_send![&*descriptor, setSizeInMillimeters: SizeInMillimeters {
                width: 508.0,
                height: 285.75,
            }];
            let _: () = msg_send![&*descriptor, setVendorID: 0x4c5au32];
            let _: () = msg_send![&*descriptor, setProductID: 0x4c52u32];
            let _: () = msg_send![&*descriptor, setSerialNum: 1u32];

            let display: objc2::rc::Allocated<AnyObject> = msg_send![classes.display, alloc];
            let object: Option<Retained<AnyObject>> =
                msg_send![display, initWithDescriptor: &*descriptor];
            let object = object.context("macOS did not create a virtual display")?;
            let id: u32 = msg_send![&*object, displayID];
            ensure!(id != 0, "macOS virtual display has an invalid identifier");
            OWNED_DISPLAYS
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(id, INITIAL_MODE);
            let display = Self {
                id,
                object: Some(object),
                mode: INITIAL_MODE,
            };
            display.apply_mode(INITIAL_MODE)?;
            Ok(display)
        }
    }

    fn resize(&mut self, mode: DisplayMode) -> Result<()> {
        check_available(&MacosPlatform::default())?;
        ensure!(
            is_extended(self.id),
            "macOS virtual display must be an extended display"
        );
        if self.mode == mode {
            return Ok(());
        }
        self.apply_mode(mode)?;
        self.mode = mode;
        OWNED_DISPLAYS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(self.id, mode);
        Ok(())
    }

    fn apply_mode(&self, resolution: DisplayMode) -> Result<()> {
        let classes = Classes::load()?;
        let object = self
            .object
            .as_deref()
            .context("macOS virtual display was removed")?;
        let scale = resolution.scale as usize;
        unsafe {
            let mode: objc2::rc::Allocated<AnyObject> = msg_send![classes.mode, alloc];
            let mode: Option<Retained<AnyObject>> = msg_send![mode,
                initWithWidth: resolution.width as usize / scale,
                height: resolution.height as usize / scale,
                refreshRate: 60.0f64
            ];
            let mode = mode.context("create macOS virtual display mode")?;
            let modes: Option<Retained<AnyObject>> = msg_send![
                AnyClass::get(c"NSArray").context("NSArray unavailable")?,
                arrayWithObject: &*mode
            ];
            let modes = modes.context("create macOS virtual display mode list")?;
            let settings: Option<Retained<AnyObject>> = msg_send![classes.settings, new];
            let settings = settings.context("create macOS virtual display settings")?;
            let _: () = msg_send![&*settings, setModes: &*modes];
            let _: () = msg_send![&*settings, setHiDPI: u32::from(scale == 2)];
            let applied: Bool = msg_send![object, applySettings: &*settings];
            ensure!(
                applied.as_bool(),
                "macOS rejected the virtual display settings"
            );
            Ok(())
        }
    }
}

impl Drop for OwnedDisplay {
    fn drop(&mut self) {
        drop(self.object.take());
        OWNED_DISPLAYS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.id);
    }
}

unsafe extern "C" {
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut AnyObject;
    fn CGDisplayIsInMirrorSet(display: u32) -> u32;
}
