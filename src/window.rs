//! Window creation module

use std::{
    time::Duration,
    fmt,
    rc::Rc
};
use webrender::{
    api::*,
    Renderer, RendererOptions, RendererKind,
    // renderer::RendererError; -- not currently public in WebRender
};
use glium::{
    IncompatibleOpenGl, Display,
    debug::DebugCallbackBehavior,
    glutin::{self, EventsLoop, AvailableMonitorsIter, GlProfile, GlContext, GlWindow, CreationError,
             MonitorId, EventsLoopProxy, ContextError, ContextBuilder, WindowBuilder},
    backend::{Context, Facade, glutin::DisplayCreationError},
};
use gleam::gl::{self, Gl};
use euclid::TypedScale;
use cassowary::{
    Variable, Solver,
    strength::*,
};

use {
    dom::Texture,
    css::{Css, FakeCss},
    window_state::{WindowState, MouseState, KeyboardState, WindowPosition},
    display_list::SolvedLayout,
    traits::Layout,
    cache::{EditVariableCache, DomTreeCache},
    id_tree::NodeId,
    compositor::Compositor,
    app::FrameEventInfo,
};

/// azul-internal ID for a window
#[derive(Debug, Copy, Clone, PartialOrd, Ord, PartialEq, Eq)]
pub struct WindowId {
    pub(crate) id: usize,
}

impl WindowId {
    pub fn new(id: usize) -> Self { Self { id: id } }
}

/// User-modifiable fake window
#[derive(Clone)]
pub struct FakeWindow {
    /// The CSS (use this field to override dynamic CSS ids).
    pub css: FakeCss,
    /// The window state for the next frame
    pub state: WindowState,
    /// An Rc to the original WindowContext - this is only so that
    /// the user can create textures and other OpenGL content in the window
    /// but not change any window properties from underneath - this would
    /// lead to mismatch between the
    pub(crate) read_only_window: Rc<Display>,
}

impl FakeWindow {
    /// Returns a read-only window which can be used to create / draw
    /// custom OpenGL texture during the `.layout()` phase
    pub fn get_window(&self) -> ReadOnlyWindow {
        ReadOnlyWindow {
            inner: self.read_only_window.clone()
        }
    }

    pub(crate) fn set_keyboard_state(&mut self, kb: &KeyboardState) {
        self.state.keyboard_state = kb.clone();
    }

    pub(crate) fn set_mouse_state(&mut self, mouse: &MouseState) {
        self.state.mouse_state = *mouse;
    }

    /// Returns a copy of the current keyboard keyboard state. We don't want the library
    /// user to be able to modify this state, only to read it.
    pub fn get_keyboard_state(&self) -> KeyboardState {
        self.state.keyboard_state.clone()
    }

    /// Returns a copy of the current windows mouse state. We don't want the library
    /// user to be able to modify this state, only to read it
    pub fn get_mouse_state(&self) -> MouseState {
        self.state.mouse_state
    }

}

/// Read-only window which can be used to create / draw
/// custom OpenGL texture during the `.layout()` phase
pub struct ReadOnlyWindow {
    pub(crate) inner: Rc<Display>,
}

impl Facade for ReadOnlyWindow {
    fn get_context(&self) -> &Rc<Context> {
        self.inner.get_context()
    }
}

use glium::{Vertex, VertexBuffer, IndexBuffer, index::PrimitiveType};
use glium::vertex::BufferCreationError as VertexBufferCreationError;
use glium::index::BufferCreationError as IndexBufferCreationError;

impl ReadOnlyWindow {
    // Since webrender is asynchronous, we can't let the user draw
    // directly onto the frame or the texture since that has to be timed
    // with webrender
    pub fn create_texture(&self, width: u32, height: u32) -> Texture {
        use glium::texture::texture2d::Texture2d;
        let tex = Texture2d::empty(&*self.inner, width, height).unwrap();
        Texture::new(tex)
    }

    /// Make the window active (OpenGL) - necessary before
    /// starting to draw on any window-owned texture
    pub fn make_current(&self) {
        unsafe {
            use glium::glutin::GlContext;
            self.inner.gl_window().make_current().unwrap();
        }
    }

    /// Unbind the current framebuffer manually. Is also executed on `Drop`.
    ///
    /// TODO: Is it necessary to expose this or is it enough to just
    /// unbind the framebuffer on drop?
    pub fn unbind_framebuffer(&self) {
        let gl = match self.inner.gl_window().get_api() {
            glutin::Api::OpenGl => unsafe {
                gl::GlFns::load_with(|symbol|
                    self.inner.gl_window().get_proc_address(symbol) as *const _)
            },
            glutin::Api::OpenGlEs => unsafe {
                gl::GlesFns::load_with(|symbol|
                    self.inner.gl_window().get_proc_address(symbol) as *const _)
            },
            glutin::Api::WebGl => unreachable!(),
        };

        gl.bind_framebuffer(gl::FRAMEBUFFER, 0);
    }
}

impl Drop for ReadOnlyWindow {
    fn drop(&mut self) {
        self.unbind_framebuffer();
    }
}

pub struct WindowInfo {
    pub window_id: WindowId,
    pub window: ReadOnlyWindow,
}

impl fmt::Debug for FakeWindow {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f,
            "FakeWindow {{\
                css: {:?}, \
                state: {:?}, \
                read_only_window: Rc<Display>, \
            }}", self.css, self.state)
    }
}

/// Window event that is passed to the user when a callback is invoked
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct WindowEvent {
    /// The ID of the window that the event was clicked on (for indexing into
    /// `app_state.windows`). `app_state.windows[event.window]` should never panic.
    pub window: usize,
    /// The nth child of the parent DOM node will generate a value of `Some(n)`
    /// when it is hit - i.e. if an element is hit, this number is set to
    ///
    /// Is set to `None` if the hit element is the root of the window
    /// (since the root node obviously has no parent).
    pub number_of_previous_siblings: Option<usize>,
    /// The (x, y) position of the mouse cursor, **relative to top left of the element that was hit**.
    pub cursor_relative_to_item: (f32, f32),
    /// The (x, y) position of the mouse cursor, **relative to top left of the window**.
    pub cursor_in_viewport: (f32, f32),
}

impl WindowEvent {
    // Mock window event, used for testing / calling callbacks without a window
    pub fn mock() -> Self {
        Self {
            window: 0,
            number_of_previous_siblings: None,
            cursor_relative_to_item: (0.0, 0.0),
            cursor_in_viewport: (0.0, 0.0),
        }
    }
}

/// Options on how to initially create the window
#[derive(Debug, Clone)]
pub struct WindowCreateOptions {
    /// State of the window, set the initial title / width / height here.
    pub state: WindowState,
    /// OpenGL clear color
    pub background: ColorF,
    /// Clear the stencil buffer with the given value. If not set, stencil buffer is not cleared
    pub clear_stencil: Option<i32>,
    /// Clear the depth buffer with the given value. If not set, depth buffer is not cleared
    pub clear_depth: Option<f32>,
    /// How should the screen be updated - as fast as possible
    /// or retained & energy saving?
    pub update_mode: UpdateMode,
    /// Which monitor should the window be created on?
    pub monitor: WindowMonitorTarget,
    /// How precise should the mouse updates be?
    pub mouse_mode: MouseMode,
    /// Should the window update regardless if the mouse is hovering
    /// over the window? (useful for games vs. applications)
    pub update_behaviour: UpdateBehaviour,
    /// Renderer type: Hardware-with-software-fallback, pure software or pure hardware renderer?
    pub renderer_type: RendererType,
}

impl Default for WindowCreateOptions {
    fn default() -> Self {
        Self {
            state: WindowState::default(),
            background: ColorF::new(1.0, 1.0, 1.0, 1.0),
            clear_stencil: None,
            clear_depth: None,
            update_mode: UpdateMode::default(),
            monitor: WindowMonitorTarget::default(),
            mouse_mode: MouseMode::default(),
            update_behaviour: UpdateBehaviour::default(),
            renderer_type: RendererType::default(),
        }
    }
}

/// Force a specific renderer.
/// By default, azul will try to use the hardware renderer and fall
/// back to the software renderer if it can't create an OpenGL 3.2 context.
/// However, in some cases a hardware renderer might create problems
/// or you want to force either a software or hardware renderer.
///
/// If the field `renderer_type` on the `WindowCreateOptions` is not
/// `RendererType::Default`, the `create_window` method will try to create
/// a window with the specific renderer type and **crash** if the renderer is
/// not available for whatever reason.
///
/// If you don't know what any of this means, leave it at `Default`.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RendererType {
    Default,
    Hardware,
    Software,
}

impl Default for RendererType {
    fn default() -> Self {
        RendererType::Default
    }
}

/// Should the window be updated only if the mouse cursor is hovering over it?
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum UpdateBehaviour {
    /// Redraw the window only if the mouse cursor is
    /// on top of the window
    UpdateOnHover,
    /// Always update the screen, regardless of the
    /// position of the mouse cursor
    UpdateAlways,
}

impl Default for UpdateBehaviour {
    fn default() -> Self {
        UpdateBehaviour::UpdateOnHover
    }
}

/// In which intervals should the screen be updated
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum UpdateMode {
    /// Retained = the screen is only updated when necessary.
    /// Underlying GlImages will be ignored and only updated when the UI changes
    Retained,
    /// Fixed update every X duration.
    FixedUpdate(Duration),
    /// Draw the screen as fast as possible.
    AsFastAsPossible,
}

impl Default for UpdateMode {
    fn default() -> Self {
        UpdateMode::Retained
    }
}

/// Mouse configuration
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MouseMode {
    /// A mouse event is only fired if the cursor has moved at least 1px.
    /// More energy saving, but less precision.
    Normal,
    /// High-precision mouse input (useful for games)
    ///
    /// This disables acceleration and uses the raw values
    /// provided by the mouse.
    DirectInput,
}

impl Default for MouseMode {
    fn default() -> Self {
        MouseMode::Normal
    }
}

/// Error that could happen during window creation
#[derive(Debug)]
pub enum WindowCreateError {
    /// WebGl is not supported by webrender
    WebGlNotSupported,
    /// Couldn't create the display from the window and the EventsLoop
    DisplayCreateError(DisplayCreationError),
    /// OpenGL version is either too old or invalid
    Gl(IncompatibleOpenGl),
    /// Could not create an OpenGL context
    Context(ContextError),
    /// Could not create a window
    CreateError(CreationError),
    /// Could not swap the front & back buffers
    SwapBuffers(::glium::SwapBuffersError),
    /// IO error
    Io(::std::io::Error),
    /// Webrender creation error (probably OpenGL missing?)
    Renderer/*(RendererError)*/,
}

impl From<::glium::SwapBuffersError> for WindowCreateError {
    fn from(e: ::glium::SwapBuffersError) -> Self {
        WindowCreateError::SwapBuffers(e)
    }
}

impl From<CreationError> for WindowCreateError {
    fn from(e: CreationError) -> Self {
        WindowCreateError::CreateError(e)
    }
}

impl From<::std::io::Error> for WindowCreateError {
    fn from(e: ::std::io::Error) -> Self {
        WindowCreateError::Io(e)
    }
}

impl From<IncompatibleOpenGl> for WindowCreateError {
    fn from(e: IncompatibleOpenGl) -> Self {
        WindowCreateError::Gl(e)
    }
}

impl From<DisplayCreationError> for WindowCreateError {
    fn from(e: DisplayCreationError) -> Self {
        WindowCreateError::DisplayCreateError(e)
    }
}

impl From<ContextError> for WindowCreateError {
    fn from(e: ContextError) -> Self {
        WindowCreateError::Context(e)
    }
}

struct Notifier {
    events_loop_proxy: EventsLoopProxy,
}

// For some reason, the wayland implementation has problems with this (?)
// However, the glium documentation explicitly says that EventsLoopProxy can
// be shared across threads.
//
// This was working absolutely fine before #cc6a8b, so I don't really get why
// this code suddenly doesn't compile anymore on wayland - I didn't even touch
// the code related to the notifier in months and didn't change the glium version
// and now it suddenly doesn't want to compile anymore.
unsafe impl Send for Notifier { }
unsafe impl Sync for Notifier { }

impl Notifier {
    fn new(events_loop_proxy: EventsLoopProxy) -> Notifier {
        Notifier {
            events_loop_proxy
        }
    }
}

impl RenderNotifier for Notifier {
    fn clone(&self) -> Box<RenderNotifier> {
        Box::new(Notifier {
            events_loop_proxy: self.events_loop_proxy.clone(),
        })
    }

    fn wake_up(&self) {
        #[cfg(not(target_os = "android"))]
        self.events_loop_proxy.wakeup().unwrap_or_else(|_| { eprintln!("couldn't wakeup event loop"); });
    }

    fn new_frame_ready(&self, _id: DocumentId, _scrolled: bool, _composite_needed: bool, _render_time: Option<u64>) {
        self.wake_up();
    }
}

/// Iterator over connected monitors (for positioning, etc.)
pub struct MonitorIter {
    inner: AvailableMonitorsIter,
}

impl Iterator for MonitorIter {
    type Item = MonitorId;
    fn next(&mut self) -> Option<MonitorId> {
        self.inner.next()
    }
}

/// Select on which monitor the window should pop up.
#[derive(Clone)]
pub enum WindowMonitorTarget {
    /// Window should appear on the primary monitor
    Primary,
    /// Use `Window::get_available_monitors()` to select the correct monitor
    Custom(MonitorId)
}

impl fmt::Debug for WindowMonitorTarget {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::WindowMonitorTarget::*;
        match *self {
            Primary =>  write!(f, "WindowMonitorTarget::Primary"),
            Custom(_) =>  write!(f, "WindowMonitorTarget::Custom(_)"),
        }
    }
}

impl Default for WindowMonitorTarget {
    fn default() -> Self {
        WindowMonitorTarget::Primary
    }
}

/// Represents one graphical window to be rendered
pub struct Window<T: Layout> {
    // TODO: technically, having one EventsLoop for all windows is sufficient
    pub(crate) events_loop: EventsLoop,
    /// Current state of the window, stores the keyboard / mouse state,
    /// visibility of the window, etc. of the LAST frame. The user never sets this
    /// field directly, but rather sets the WindowState he wants to have for the NEXT frame,
    /// then azul compares the changes (i.e. if we are currently in fullscreen mode and
    /// the user wants the next screen to be in fullscreen mode, too, simply do nothing), then it
    /// updates this field to reflect the changes.
    ///
    /// This field is initialized from the `WindowCreateOptions`.
    pub(crate) state: WindowState,
    /// The webrender renderer
    pub(crate) renderer: Option<Renderer>,
    /// The display, i.e. the window
    pub(crate) display: Rc<Display>,
    /// The `WindowInternal` allows us to solve some borrowing issues
    pub(crate) internal: WindowInternal,
    /// The solver for the UI, for caching the results of the computations
    pub(crate) solver: UiSolver<T>,
    // The background thread that is running for this window.
    // pub(crate) background_thread: Option<JoinHandle<()>>,
    /// The css (how the current window is styled)
    pub css: Css,
}

/// Used in the solver, for the root constraint
#[derive(Debug, Copy, Clone, PartialEq)]
pub(crate) struct WindowDimensions {
    pub(crate) layout_size: LayoutSize,
    pub(crate) width_var: Variable,
    pub(crate) height_var: Variable,
}

impl WindowDimensions {
    pub fn new_from_layout_size(layout_size: LayoutSize) -> Self {
        Self {
            layout_size: layout_size,
            width_var: Variable::new(),
            height_var: Variable::new(),
        }
    }

    pub fn width(&self) -> f32 {
        self.layout_size.width_typed().get()
    }
    pub fn height(&self) -> f32 {
        self.layout_size.height_typed().get()
    }
}

/// Solver for solving the UI of the current window
pub(crate) struct UiSolver<T: Layout> {
    /// The actual solver
    pub(crate) solver: Solver,
    /// Solved layout from the previous frame (empty by default)
    /// This is necessary for caching the constraints of the given layout
    pub(crate) solved_layout: SolvedLayout<T>,
    /// The list of variables that has been added to the solver
    pub(crate) edit_variable_cache: EditVariableCache,
    /// The cache of the previous frames DOM tree
    pub(crate) dom_tree_cache: DomTreeCache,
}

impl<T: Layout> UiSolver<T> {
    pub(crate) fn query_bounds_of_rect(&self, rect_id: NodeId) {
        // TODO: After solving the UI, use this function to get the actual coordinates of an item in the UI.
        // This function should cache values accordingly
    }
}

pub(crate) struct WindowInternal {
    pub(crate) last_display_list_builder: BuiltDisplayList,
    pub(crate) api: RenderApi,
    pub(crate) epoch: Epoch,
    pub(crate) pipeline_id: PipelineId,
    pub(crate) document_id: DocumentId,
}

impl<T: Layout> Window<T> {

    /// Creates a new window
    pub fn new(options: WindowCreateOptions, css: Css) -> Result<Self, WindowCreateError>  {

        let events_loop = EventsLoop::new();

        let mut window = WindowBuilder::new()
            .with_dimensions(options.state.size.width, options.state.size.height)
            .with_title(options.state.title.clone())
            .with_decorations(options.state.has_decorations)
            .with_visibility(options.state.is_visible)
            .with_transparency(options.state.is_transparent)
            .with_maximized(options.state.is_maximized)
            .with_multitouch();

        // TODO: Update winit to have:
        //      .with_always_on_top(options.state.is_always_on_top)
        //
        // winit 0.13 -> winit 0.15

        // TODO: Add all the extensions for X11 / Mac / Windows,
        // like setting the taskbar icon, setting the titlebar icon, etc.

        if options.state.is_fullscreen {
            let monitor = match options.monitor {
                WindowMonitorTarget::Primary => events_loop.get_primary_monitor(),
                WindowMonitorTarget::Custom(ref id) => id.clone(),
            };

            window = window.with_fullscreen(Some(monitor));
        }

        if let Some((min_w, min_h)) = options.state.size.min_dimensions {
            window = window.with_min_dimensions(min_w, min_h);
        }

        if let Some((max_w, max_h)) = options.state.size.max_dimensions {
            window = window.with_max_dimensions(max_w, max_h);
        }

        fn create_context_builder<'a>(vsync: bool, srgb: bool) -> ContextBuilder<'a> {
            let mut builder = ContextBuilder::new()
                .with_gl(glutin::GlRequest::GlThenGles {
                    opengl_version: (3, 2),
                    opengles_version: (3, 0),
                })
                .with_gl_profile(GlProfile::Core);

            #[cfg(debug_assertions)] {
                builder = builder.with_gl_debug_flag(true);
            }

            #[cfg(not(debug_assertions))] {
                builder = builder.with_gl_debug_flag(false);
            }

            if vsync {
                builder = builder.with_vsync(true);
            }
            if srgb {
                builder = builder.with_srgb(true);
            }
            builder
        }

        // Only create a context with VSync and SRGB if the context creation works
        let gl_window = GlWindow::new(window.clone(), create_context_builder(true, true), &events_loop)
            .or_else(|_| GlWindow::new(window.clone(), create_context_builder(true, false), &events_loop))
            .or_else(|_| GlWindow::new(window.clone(), create_context_builder(false, true), &events_loop))
            .or_else(|_| GlWindow::new(window, create_context_builder(false, false), &events_loop))?;

        if let Some(WindowPosition { x, y }) = options.state.position {
            gl_window.window().set_position(x as i32, y as i32);
        }

        #[cfg(debug_assertions)]
        let display = Display::with_debug(gl_window, DebugCallbackBehavior::DebugMessageOnError)?;
        #[cfg(not(debug_assertions))]
        let display = Display::with_debug(gl_window, DebugCallbackBehavior::Ignore)?;

        let device_pixel_ratio = display.gl_window().hidpi_factor();

        // this exists because RendererOptions isn't Clone-able
        fn get_renderer_opts(native: bool, device_pixel_ratio: f32, clear_color: Option<ColorF>) -> RendererOptions {
            use webrender::ProgramCache;
            RendererOptions {
                resource_override_path: None,
                // pre-caching shaders means to compile all shaders on startup
                // this can take significant time and should be only used for testing the shaders
                precache_shaders: false,
                device_pixel_ratio: device_pixel_ratio,
                enable_subpixel_aa: true,
                enable_aa: true,
                clear_color: clear_color,
                enable_render_on_scroll: true,
                enable_scrollbars: true,
                cached_programs: Some(ProgramCache::new(None)),
                renderer_kind: if native {
                    RendererKind::Native
                } else {
                    RendererKind::OSMesa
                },
                .. RendererOptions::default()
            }
        }

        let framebuffer_size = {
            #[allow(deprecated)]
            let (width, height) = display.gl_window().get_inner_size_pixels().unwrap();
            DeviceUintSize::new(width, height)
        };

        let notifier = Box::new(Notifier::new(events_loop.create_proxy()));

        let gl = get_gl_context(&display)?;

        let opts_native = get_renderer_opts(true, device_pixel_ratio, Some(options.background));
        let opts_osmesa = get_renderer_opts(false, device_pixel_ratio, Some(options.background));

        use self::RendererType::*;
        let (mut renderer, sender) = match options.renderer_type {
            Hardware => {
                // force hardware renderer
                Renderer::new(gl, notifier, opts_native).unwrap()
            },
            Software => {
                // force software renderer
                Renderer::new(gl, notifier, opts_osmesa).unwrap()
            },
            Default => {
                // try hardware first, fall back to software
                Renderer::new(gl.clone(), notifier.clone(), opts_native).or_else(|_|
                Renderer::new(gl, notifier, opts_osmesa)).unwrap()
            }
        };

        let api = sender.create_api();
        let document_id = api.add_document(framebuffer_size, 0);
        let epoch = Epoch(0);
        let pipeline_id = PipelineId(0, 0);
        let layout_size = framebuffer_size.to_f32() / TypedScale::new(device_pixel_ratio);
/*
        let (sender, receiver) = channel();
        let thread = Builder::new().name(options.title.clone()).spawn(move || Self::handle_event(receiver))?;
*/
        let mut solver = Solver::new();

        let window_dim = WindowDimensions::new_from_layout_size(layout_size);

        solver.add_edit_variable(window_dim.width_var, STRONG).unwrap();
        solver.add_edit_variable(window_dim.height_var, STRONG).unwrap();
        solver.suggest_value(window_dim.width_var, window_dim.width() as f64).unwrap();
        solver.suggest_value(window_dim.height_var, window_dim.height() as f64).unwrap();

        renderer.set_external_image_handler(Box::new(Compositor::default()));

        let window = Window {
            events_loop: events_loop,
            state: options.state,
            renderer: Some(renderer),
            display: Rc::new(display),
            css: css,
            internal: WindowInternal {
                api: api,
                epoch: epoch,
                pipeline_id: pipeline_id,
                document_id: document_id,
                last_display_list_builder: BuiltDisplayList::default(),
            },
            solver: UiSolver {
                solver: solver,
                solved_layout: SolvedLayout::empty(),
                edit_variable_cache: EditVariableCache::empty(),
                dom_tree_cache: DomTreeCache::empty(),
            }
        };

        Ok(window)
    }

    pub fn get_available_monitors() -> MonitorIter {
        MonitorIter {
            inner: EventsLoop::new().get_available_monitors(),
        }
    }

    /// Updates the window state, diff the `self.state` with the `new_state`
    /// and updating the platform window to reflect the changes
    ///
    /// Note: Currently, setting `mouse_state.position`, `window.size` or
    /// `window.position` has no effect on the platform window, since they are very
    /// frequently modified by the user (other properties are always set by the
    /// application developer)
    pub(crate) fn update_from_user_window_state(&mut self, new_state: WindowState) {

        let gl_window = self.display.gl_window();
        let window = gl_window.window();
        let old_state = &mut self.state;

        // Compare the old and new state, field by field

        if old_state.title != new_state.title {
            window.set_title(&new_state.title);
            old_state.title = new_state.title;
        }

        if old_state.mouse_state.mouse_cursor_type != new_state.mouse_state.mouse_cursor_type {
            window.set_cursor(new_state.mouse_state.mouse_cursor_type);
            old_state.mouse_state.mouse_cursor_type = new_state.mouse_state.mouse_cursor_type;
        }

        if old_state.is_maximized != new_state.is_maximized {
            window.set_maximized(new_state.is_maximized);
            old_state.is_maximized = new_state.is_maximized;
        }

        if old_state.is_fullscreen != new_state.is_fullscreen {
            if new_state.is_fullscreen {
                window.set_fullscreen(Some(window.get_current_monitor()));
            } else {
                window.set_fullscreen(None);
            }
            old_state.is_fullscreen = new_state.is_fullscreen;
        }

        if old_state.has_decorations != new_state.has_decorations {
            window.set_decorations(new_state.has_decorations);
            old_state.has_decorations = new_state.has_decorations;
        }

        if old_state.is_visible != new_state.is_visible {
            if new_state.is_visible {
                window.show();
            } else {
                window.hide();
            }
            old_state.is_visible = new_state.is_visible;
        }

        if old_state.size.min_dimensions != new_state.size.min_dimensions {
            window.set_min_dimensions(new_state.size.min_dimensions);
            old_state.size.min_dimensions = new_state.size.min_dimensions;
        }

        if old_state.size.max_dimensions != new_state.size.max_dimensions {
            window.set_max_dimensions(new_state.size.max_dimensions);
            old_state.size.max_dimensions = new_state.size.max_dimensions;
        }
    }

    pub(crate) fn update_from_external_window_state(&mut self, frame_event_info: &mut FrameEventInfo) {
        use webrender::api::{DeviceUintSize, WorldPoint, LayoutSize};

        if let Some((w, h)) = frame_event_info.new_window_size {
            self.state.size.width = w;
            self.state.size.height = h;
            frame_event_info.should_redraw_window = true;
        }

        if let Some(dpi) = frame_event_info.new_dpi_factor {
            self.state.size.hidpi_factor = dpi;
            frame_event_info.should_redraw_window = true;
        }
    }

    /// Resets the mouse states `scroll_x` and `scroll_y` to 0
    pub(crate) fn clear_scroll_state(&mut self) {
        self.state.mouse_state.scroll_x = 0.0;
        self.state.mouse_state.scroll_y = 0.0;
    }
}

pub(crate) fn get_gl_context(display: &Display) -> Result<Rc<Gl>, WindowCreateError> {
    match display.gl_window().get_api() {
        glutin::Api::OpenGl => Ok(unsafe {
            gl::GlFns::load_with(|symbol| display.gl_window().get_proc_address(symbol) as *const _)
        }),
        glutin::Api::OpenGlEs => Ok(unsafe {
            gl::GlesFns::load_with(|symbol| display.gl_window().get_proc_address(symbol) as *const _)
        }),
        glutin::Api::WebGl => Err(WindowCreateError::WebGlNotSupported),
    }
}

impl<T: Layout> Drop for Window<T> {
    fn drop(&mut self) {
        // self.background_thread.take().unwrap().join();
        let renderer = self.renderer.take().unwrap();
        renderer.deinit();
    }
}

// Empty test, for some reason codecov doesn't detect any files (and therefore
// doesn't report codecov % correctly) except if they have at least one test in
// the file. This is an empty test, which should be updated later on
#[test]
fn __codecov_test_window_file() {

}