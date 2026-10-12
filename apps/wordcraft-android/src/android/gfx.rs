//! Drawing egui frames with wgpu on an Android window.
//!
//! Vulkan first, then OpenGL ES, each in an instance of its own: a Vulkan surface connects the window,
//! after which EGL can't use it. Every wgpu call that can panic on a bad state is guarded: the device
//! gets an error handler (wgpu's default panics), a surface is configured inside error scopes and only
//! used once configuring succeeded, and painting runs under `catch_unwind` as a last resort.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use egui_wgpu::wgpu;
use ndk::native_window::NativeWindow;
use wgpu::rwh::{DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle};

/// Which wgpu backend to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Vulkan, falling back to OpenGL ES.
    Auto,
    Vulkan,
    Gl,
}

impl Backend {
    /// From the `--backend vulkan|gl` launch flag (for measuring both).
    pub fn parse(args: &str) -> Self {
        let mut words = args.split_whitespace();
        while let Some(w) = words.next() {
            if w == "--backend" {
                return match words.next() {
                    Some("vulkan") => Self::Vulkan,
                    Some("gl") => Self::Gl,
                    _ => Self::Auto,
                };
            }
        }
        Self::Auto
    }

    fn candidates(self) -> &'static [wgpu::Backends] {
        match self {
            Self::Auto => &[wgpu::Backends::VULKAN, wgpu::Backends::GL],
            Self::Vulkan => &[wgpu::Backends::VULKAN],
            Self::Gl => &[wgpu::Backends::GL],
        }
    }
}

/// A window as wgpu wants it: ndk gives the window handle; Android's display handle carries no data.
#[derive(Debug, Clone)]
struct Target(NativeWindow);

impl HasWindowHandle for Target {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        self.0.window_handle()
    }
}

impl HasDisplayHandle for Target {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::android())
    }
}

/// The window's size in pixels, if it has one.
pub fn window_size(window: &NativeWindow) -> Option<[u32; 2]> {
    let w = u32::try_from(window.width()).ok().filter(|&v| v > 0)?;
    let h = u32::try_from(window.height()).ok().filter(|&v| v > 0)?;
    Some([w, h])
}

/// Never call twice for one window without dropping the first surface: the Vulkan backend `expect`s
/// that connecting the window works.
fn create_surface(instance: &wgpu::Instance, window: &NativeWindow) -> Result<wgpu::Surface<'static>, String> {
    match catch_unwind(AssertUnwindSafe(|| instance.create_surface(Target(window.clone())))) {
        Ok(Ok(surface)) => Ok(surface),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("creating the surface panicked".to_owned()),
    }
}

/// Drops `value` under `catch_unwind`: false if dropping it panicked (a driver's teardown).
pub fn drop_guarded<T>(value: T) -> bool {
    catch_unwind(AssertUnwindSafe(move || drop(value))).is_ok()
}

/// The surface of the current window. Drop it before the window goes (inside `TerminateWindow`).
pub struct WindowSurface {
    surface: wgpu::Surface<'static>,
    /// Set only once configuring succeeded: wgpu panics on acquiring from an unconfigured surface.
    config: Option<wgpu::SurfaceConfiguration>,
}

/// What painting a frame did.
#[derive(Debug, PartialEq, Eq)]
pub enum Painted {
    Presented,
    /// No frame this time (no surface, the window is resizing, the swapchain timed out…).
    Skipped,
    /// The surface is gone: drop it and attach the window again.
    RecreateSurface,
    /// The device is gone: drop everything, start a new [`Gpu`] and a new egui context.
    Reinit,
}

/// The device and egui's renderer, kept while the window comes and goes.
pub struct Gpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: egui_wgpu::Renderer,
    format: wgpu::TextureFormat,
    lost: Arc<AtomicBool>,
    /// The last problem logged while drawing: a problem that repeats every frame is logged once.
    last_warning: Option<String>,
}

impl Gpu {
    /// A device that can draw to `window`, with that window's surface.
    pub fn new(window: &NativeWindow, backend: Backend) -> Result<(Self, WindowSurface), String> {
        let mut last = "no graphics backend".to_owned();
        for &backends in backend.candidates() {
            match Self::with_backends(backends, window) {
                Ok(ok) => return Ok(ok),
                Err(e) => {
                    log::warn!("wgpu {backends:?} unusable: {e}");
                    last = e;
                }
            }
        }
        Err(last)
    }

    fn with_backends(backends: wgpu::Backends, window: &NativeWindow) -> Result<(Self, WindowSurface), String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            // Never the debug flags, even in debug builds: naming Vulkan objects (`DEBUG`) crashes some
            // drivers, the emulator's among them, and validation layers aren't on devices anyway.
            flags: wgpu::InstanceFlags::from_build_config()
                - (wgpu::InstanceFlags::DEBUG | wgpu::InstanceFlags::VALIDATION | wgpu::InstanceFlags::GPU_BASED_VALIDATION),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            backend_options: wgpu::BackendOptions::default(),
            display: None,
        });
        let surface = create_surface(&instance, window)?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))
        .map_err(|e| e.to_string())?;
        let info = adapter.get_info();
        // Mobile GPUs: ask for the downlevel limits, plus the largest textures the GPU has (the window
        // and page images can be bigger than the downlevel 2048).
        let base = if info.backend == wgpu::Backend::Gl { wgpu::Limits::downlevel_webgl2_defaults() } else { wgpu::Limits::downlevel_defaults() };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("wordcraft"),
            required_features: wgpu::Features::empty(),
            required_limits: base.using_resolution(adapter.limits()),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            // `Performance` allocates Vulkan memory in blocks of at least 128 MB.
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            trace: wgpu::Trace::Off,
        }))
        .map_err(|e| e.to_string())?;
        // wgpu's default handler panics on any error.
        device.on_uncaptured_error(Arc::new(|err: wgpu::Error| log::error!("wgpu: {err}")));
        let lost = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&lost);
        device.set_device_lost_callback(move |reason, message| {
            log::error!("wgpu device lost ({reason:?}): {message}");
            flag.store(true, Ordering::Release);
        });
        let formats = surface.get_capabilities(&adapter).formats;
        // egui draws in gamma space: a non-sRGB framebuffer.
        let format = egui_wgpu::preferred_framebuffer_format(&formats).map_err(|e| e.to_string())?;
        let renderer = egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        log::info!("wgpu {:?} on {} ({}), {format:?}", info.backend, info.name, info.driver_info);
        let gpu = Self { instance, adapter, device, queue, renderer, format, lost, last_warning: None };
        let mut ws = WindowSurface { surface, config: None };
        gpu.configure(&mut ws, window)?;
        Ok((gpu, ws))
    }

    /// The largest texture the device takes.
    pub fn max_texture_side(&self) -> usize {
        usize::try_from(self.device.limits().max_texture_dimension_2d).unwrap_or(2048)
    }

    /// A new window (after the previous surface was dropped).
    pub fn attach(&self, window: &NativeWindow) -> Result<WindowSurface, String> {
        let surface = create_surface(&self.instance, window)?;
        if !self.adapter.is_surface_supported(&surface) {
            return Err("the graphics device can't draw to this window".to_owned());
        }
        let mut ws = WindowSurface { surface, config: None };
        self.configure(&mut ws, window)?;
        Ok(ws)
    }

    fn configure(&self, ws: &mut WindowSurface, window: &NativeWindow) -> Result<(), String> {
        ws.config = None;
        let [width, height] = window_size(window).ok_or("the window has no size")?;
        let max = self.device.limits().max_texture_dimension_2d;
        if width > max || height > max {
            return Err(format!("the window ({width}×{height}) is larger than the GPU's {max}"));
        }
        if !ws.surface.get_capabilities(&self.adapter).formats.contains(&self.format) {
            return Err(format!("the window no longer offers {:?}", self.format));
        }
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width,
            height,
            // Fifo (the only mode every Android device has), with the shortest queue the device allows:
            // the lowest typing latency.
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 1,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: Vec::new(),
        };
        let oom = self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let internal = self.device.push_error_scope(wgpu::ErrorFilter::Internal);
        let validation = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        ws.surface.configure(&self.device, &config);
        let errors = [pollster::block_on(validation.pop()), pollster::block_on(internal.pop()), pollster::block_on(oom.pop())];
        if let Some(err) = errors.into_iter().flatten().next() {
            return Err(err.to_string());
        }
        if self.lost.load(Ordering::Acquire) {
            return Err("the graphics device was lost".to_owned());
        }
        ws.config = Some(config);
        Ok(())
    }

    /// Applies the texture changes and draws the frame (without a surface, only the textures change).
    /// `textures` is always emptied: egui never sends a texture twice.
    pub fn paint(&mut self, ws: Option<&mut WindowSurface>, window: Option<&NativeWindow>, frame: &mut PaintJob<'_>) -> Painted {
        if self.lost.load(Ordering::Acquire) {
            frame.textures.clear();
            return Painted::Reinit;
        }
        match catch_unwind(AssertUnwindSafe(|| self.paint_inner(ws, window, frame))) {
            Ok(painted) => painted,
            Err(_) => {
                log::error!("painting panicked; starting the graphics again");
                frame.textures.clear();
                Painted::Reinit
            }
        }
    }

    fn paint_inner(&mut self, ws: Option<&mut WindowSurface>, window: Option<&NativeWindow>, frame: &mut PaintJob<'_>) -> Painted {
        for (id, deltas) in frame.textures.set.drain() {
            for delta in &deltas {
                self.renderer.update_texture(&self.device, &self.queue, id, delta);
            }
        }
        let painted = match (ws, window) {
            (Some(ws), Some(window)) => self.draw(ws, window, frame),
            _ => {
                self.queue.submit([]);
                Painted::Skipped
            }
        };
        for id in frame.textures.free.drain() {
            self.renderer.free_texture(&id);
        }
        painted
    }

    fn draw(&mut self, ws: &mut WindowSurface, window: &NativeWindow, frame: &PaintJob<'_>) -> Painted {
        let skip = |queue: &wgpu::Queue| {
            queue.submit([]);
            Painted::Skipped
        };
        let Some(size) = window_size(window) else { return skip(&self.queue) };
        // Android doesn't report a resize as an outdated swapchain: compare sizes every frame.
        let configured = ws.config.as_ref().map(|c| [c.width, c.height]);
        if configured != Some(size)
            && let Err(e) = self.configure(ws, window)
        {
            self.warn(format!("configuring the surface: {e}"));
            return skip(&self.queue);
        }
        let Some(config) = ws.config.as_ref() else { return skip(&self.queue) };
        let screen = egui_wgpu::ScreenDescriptor { size_in_pixels: [config.width, config.height], pixels_per_point: frame.pixels_per_point };
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("egui") });
        let user = self.renderer.update_buffers(&self.device, &self.queue, &mut encoder, frame.primitives, &screen);
        let output = match ws.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            other => {
                self.queue.submit(user.into_iter().chain([encoder.finish()]));
                return match other {
                    wgpu::CurrentSurfaceTexture::Lost => Painted::RecreateSurface,
                    wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Validation => {
                        ws.config = None;
                        Painted::Skipped
                    }
                    _ => Painted::Skipped,
                };
            }
        };
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let [r, g, b, a] = frame.clear.to_normalized_gamma_f32();
        let clear = wgpu::Color { r: f64::from(r), g: f64::from(g), b: f64::from(b), a: f64::from(a) };
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear), store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            self.renderer.render(&mut pass, frame.primitives, &screen);
        }
        self.queue.submit(user.into_iter().chain([encoder.finish()]));
        self.queue.present(output);
        self.last_warning = None;
        Painted::Presented
    }

    fn warn(&mut self, message: String) {
        if self.last_warning.as_ref() != Some(&message) {
            log::warn!("{message}");
            self.last_warning = Some(message);
        }
    }
}

/// One frame to paint.
pub struct PaintJob<'a> {
    pub primitives: &'a [egui::ClippedPrimitive],
    pub textures: &'a mut egui::TexturesDelta,
    pub pixels_per_point: f32,
    /// What shows where egui draws nothing.
    pub clear: egui::Color32,
}
