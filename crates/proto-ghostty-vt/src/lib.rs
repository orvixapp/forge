//! Narrow, safe Rust wrapper around the unstable `libghostty-vt` C API.
//!
//! The ABI is pinned to Ghostty commit
//! `7aab0a0392369613472bd5dcfd66bef58e78c3ec`. Keeping the dynamic-loading
//! and unsafe boundary here prevents the API churn from leaking into Forge.

use libloading::Library;
use std::{ffi::c_void, path::Path, ptr, sync::Arc};
use thiserror::Error;

const GHOSTTY_SUCCESS: i32 = 0;
const GHOSTTY_OUT_OF_SPACE: i32 = -3;
const GHOSTTY_FORMATTER_FORMAT_PLAIN: i32 = 0;

type RawTerminal = *mut c_void;
type RawFormatter = *mut c_void;
type RawRenderState = *mut c_void;

type TerminalNew = unsafe extern "C" fn(*const c_void, *mut RawTerminal, u16, u16) -> i32;
type TerminalFree = unsafe extern "C" fn(RawTerminal);
type TerminalWrite = unsafe extern "C" fn(RawTerminal, *const u8, usize);
type TerminalResize = unsafe extern "C" fn(RawTerminal, u16, u16, u32, u32) -> i32;
type FormatterNew = unsafe extern "C" fn(
    *const c_void,
    *mut RawFormatter,
    RawTerminal,
    FormatterTerminalOptions,
) -> i32;
type FormatterFormatBuf = unsafe extern "C" fn(RawFormatter, *mut u8, usize, *mut usize) -> i32;
type FormatterFree = unsafe extern "C" fn(RawFormatter);
type RenderStateNew = unsafe extern "C" fn(*const c_void, *mut RawRenderState) -> i32;
type RenderStateFree = unsafe extern "C" fn(RawRenderState);
type RenderStateUpdate = unsafe extern "C" fn(RawRenderState, RawTerminal) -> i32;
type RenderStateGet = unsafe extern "C" fn(RawRenderState, i32, *mut c_void) -> i32;
type RenderStateClean = unsafe extern "C" fn(RawRenderState) -> i32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FormatterScreenExtra {
    size: usize,
    cursor: bool,
    style: bool,
    hyperlink: bool,
    protection: bool,
    kitty_keyboard: bool,
    charsets: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FormatterTerminalExtra {
    size: usize,
    palette: bool,
    modes: bool,
    scrolling_region: bool,
    tabstops: bool,
    pwd: bool,
    keyboard: bool,
    screen: FormatterScreenExtra,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FormatterTerminalOptions {
    size: usize,
    emit: i32,
    unwrap: bool,
    trim: bool,
    extra: FormatterTerminalExtra,
    selection: *const c_void,
}

#[derive(Debug, Error)]
pub enum GhosttyError {
    #[error("failed to load libghostty-vt: {0}")]
    Load(#[from] libloading::Error),
    #[error("libghostty-vt operation {operation} failed with result {result}")]
    Operation {
        operation: &'static str,
        result: i32,
    },
    #[error("libghostty-vt returned an invalid null handle for {0}")]
    NullHandle(&'static str),
}

struct Api {
    _library: Library,
    terminal_new: TerminalNew,
    terminal_free: TerminalFree,
    terminal_write: TerminalWrite,
    terminal_resize: TerminalResize,
    formatter_new: FormatterNew,
    formatter_format_buf: FormatterFormatBuf,
    formatter_free: FormatterFree,
    render_state_new: RenderStateNew,
    render_state_free: RenderStateFree,
    render_state_update: RenderStateUpdate,
    render_state_get: RenderStateGet,
    render_state_clean: RenderStateClean,
}

impl Api {
    unsafe fn load(path: &Path) -> Result<Self, GhosttyError> {
        // SAFETY: the caller selects a pinned libghostty-vt build. Each symbol
        // is copied as a function pointer while `library` remains owned by Api.
        let library = unsafe { Library::new(path)? };
        macro_rules! symbol {
            ($name:literal, $type:ty) => {{
                // SAFETY: symbol names and signatures match the pinned C headers.
                *unsafe { library.get::<$type>($name)? }
            }};
        }
        Ok(Self {
            terminal_new: symbol!(b"ghostty_terminal_new\0", TerminalNew),
            terminal_free: symbol!(b"ghostty_terminal_free\0", TerminalFree),
            terminal_write: symbol!(b"ghostty_terminal_vt_write\0", TerminalWrite),
            terminal_resize: symbol!(b"ghostty_terminal_resize\0", TerminalResize),
            formatter_new: symbol!(b"ghostty_formatter_terminal_new\0", FormatterNew),
            formatter_format_buf: symbol!(b"ghostty_formatter_format_buf\0", FormatterFormatBuf),
            formatter_free: symbol!(b"ghostty_formatter_free\0", FormatterFree),
            render_state_new: symbol!(b"ghostty_render_state_new\0", RenderStateNew),
            render_state_free: symbol!(b"ghostty_render_state_free\0", RenderStateFree),
            render_state_update: symbol!(b"ghostty_render_state_update\0", RenderStateUpdate),
            render_state_get: symbol!(b"ghostty_render_state_get\0", RenderStateGet),
            render_state_clean: symbol!(b"ghostty_render_state_clean\0", RenderStateClean),
            _library: library,
        })
    }
}

#[derive(Clone)]
pub struct GhosttyLibrary {
    api: Arc<Api>,
}

impl GhosttyLibrary {
    /// Loads the pinned `libghostty-vt` shared library.
    ///
    /// # Errors
    ///
    /// Returns an error if the library or any required ABI symbol is missing.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, GhosttyError> {
        // SAFETY: Api::load validates every symbol needed by the safe wrapper.
        let api = unsafe { Api::load(path.as_ref())? };
        Ok(Self { api: Arc::new(api) })
    }

    /// Creates a terminal state with non-zero cell dimensions.
    ///
    /// # Errors
    ///
    /// Returns an error if Ghostty rejects the dimensions or allocation fails.
    pub fn terminal(&self, cols: u16, rows: u16) -> Result<GhosttyTerminal, GhosttyError> {
        let mut raw = ptr::null_mut();
        // SAFETY: `raw` is a valid out pointer; null selects Ghostty's allocator.
        let result =
            unsafe { (self.api.terminal_new)(ptr::null(), &raw mut raw, cols.max(1), rows.max(1)) };
        check("terminal_new", result)?;
        if raw.is_null() {
            return Err(GhosttyError::NullHandle("terminal_new"));
        }
        let mut render_state = ptr::null_mut();
        // SAFETY: `render_state` is a valid out pointer and null selects the
        // default allocator. On failure the already-created terminal is freed.
        let result = unsafe {
            (self.api.render_state_new)(ptr::null(), &raw mut render_state)
        };
        if let Err(error) = check("render_state_new", result) {
            // SAFETY: `raw` was created successfully and is uniquely owned.
            unsafe { (self.api.terminal_free)(raw) };
            return Err(error);
        }
        if render_state.is_null() {
            // SAFETY: `raw` was created successfully and is uniquely owned.
            unsafe { (self.api.terminal_free)(raw) };
            return Err(GhosttyError::NullHandle("render_state_new"));
        }
        Ok(GhosttyTerminal {
            api: Arc::clone(&self.api),
            raw,
            render_state,
        })
    }
}

pub struct GhosttyTerminal {
    api: Arc<Api>,
    raw: RawTerminal,
    render_state: RawRenderState,
}

// SAFETY: libghostty-vt terminal handles have no thread affinity. Forge never
// accesses a handle concurrently; callers must move it or protect it by a mutex.
unsafe impl Send for GhosttyTerminal {}

impl GhosttyTerminal {
    pub fn write(&mut self, data: &[u8]) {
        // SAFETY: `raw` is owned and valid; the slice pointer lives for the call.
        unsafe { (self.api.terminal_write)(self.raw, data.as_ptr(), data.len()) };
    }

    /// Resizes the emulated terminal and reflows its primary screen.
    ///
    /// # Errors
    ///
    /// Returns an error if Ghostty rejects the dimensions.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), GhosttyError> {
        // Pixel dimensions are unknown in the daemon until a renderer attaches.
        let result =
            unsafe { (self.api.terminal_resize)(self.raw, cols.max(1), rows.max(1), 0, 0) };
        check("terminal_resize", result)
    }

    /// Synchronizes Ghostty's incremental render state and returns the frame
    /// metadata together with a plain-text bridge for the prototype client.
    ///
    /// # Errors
    ///
    /// Returns an error when Ghostty cannot update or query its render state.
    pub fn render_snapshot(&mut self) -> Result<RenderSnapshot, GhosttyError> {
        // SAFETY: both handles are uniquely owned by this value and valid.
        let result = unsafe { (self.api.render_state_update)(self.render_state, self.raw) };
        check("render_state_update", result)?;

        let cols = self.render_value::<u16>(1, "render_state_get(cols)")?;
        let rows = self.render_value::<u16>(2, "render_state_get(rows)")?;
        let dirty_raw = self.render_value::<i32>(3, "render_state_get(dirty)")?;
        let dirty = DirtyState::try_from(dirty_raw)?;
        let text = if dirty == DirtyState::Clean {
            None
        } else {
            Some(self.snapshot_text()?)
        };
        // SAFETY: the state is valid and the complete prototype frame was read.
        let result = unsafe { (self.api.render_state_clean)(self.render_state) };
        check("render_state_clean", result)?;
        Ok(RenderSnapshot {
            cols,
            rows,
            dirty,
            text,
        })
    }

    fn render_value<T: Default>(
        &self,
        data: i32,
        operation: &'static str,
    ) -> Result<T, GhosttyError> {
        let mut value = T::default();
        // SAFETY: each private call site pairs the data tag with its documented
        // output type from the pinned Ghostty header.
        let result = unsafe {
            (self.api.render_state_get)(
                self.render_state,
                data,
                (&raw mut value).cast::<c_void>(),
            )
        };
        check(operation, result)?;
        Ok(value)
    }

    /// Returns the active screen as plain UTF-8 text.
    ///
    /// This is a prototype bridge. The GPU stage will consume Ghostty's render
    /// state and dirty-row APIs instead of formatting the whole viewport.
    ///
    /// # Errors
    ///
    /// Returns an error if formatter allocation or output fails.
    pub fn snapshot_text(&self) -> Result<String, GhosttyError> {
        let mut formatter = ptr::null_mut();
        let options = formatter_options();
        // SAFETY: all handles and by-value ABI structs match the pinned header.
        let result =
            unsafe { (self.api.formatter_new)(ptr::null(), &raw mut formatter, self.raw, options) };
        check("formatter_terminal_new", result)?;
        if formatter.is_null() {
            return Err(GhosttyError::NullHandle("formatter_terminal_new"));
        }
        let formatter = FormatterGuard {
            api: Arc::clone(&self.api),
            raw: formatter,
        };

        let mut required = 0;
        // SAFETY: null with zero capacity is the documented size-query operation.
        let query = unsafe {
            (self.api.formatter_format_buf)(formatter.raw, ptr::null_mut(), 0, &raw mut required)
        };
        if required == 0 && query == GHOSTTY_SUCCESS {
            return Ok(String::new());
        }
        if query != GHOSTTY_OUT_OF_SPACE {
            check("formatter_format_buf(size)", query)?;
        }
        let mut bytes = vec![0_u8; required];
        let mut written = 0;
        // SAFETY: `bytes` exposes exactly the capacity passed to Ghostty.
        let result = unsafe {
            (self.api.formatter_format_buf)(
                formatter.raw,
                bytes.as_mut_ptr(),
                bytes.len(),
                &raw mut written,
            )
        };
        check("formatter_format_buf", result)?;
        bytes.truncate(written);
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

impl Drop for GhosttyTerminal {
    fn drop(&mut self) {
        // SAFETY: the render state is uniquely owned and freed before the
        // terminal it references.
        unsafe { (self.api.render_state_free)(self.render_state) };
        // SAFETY: this handle is uniquely owned and freed exactly once.
        unsafe { (self.api.terminal_free)(self.raw) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyState {
    Clean,
    Partial,
    Full,
}

impl TryFrom<i32> for DirtyState {
    type Error = GhosttyError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Clean),
            1 => Ok(Self::Partial),
            2 => Ok(Self::Full),
            result => Err(GhosttyError::Operation {
                operation: "render_state_get(dirty value)",
                result,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub dirty: DirtyState,
    /// Temporary text transport until the renderer consumes row/cell data.
    pub text: Option<String>,
}

struct FormatterGuard {
    api: Arc<Api>,
    raw: RawFormatter,
}

impl Drop for FormatterGuard {
    fn drop(&mut self) {
        // SAFETY: this temporary handle is uniquely owned and freed exactly once.
        unsafe { (self.api.formatter_free)(self.raw) };
    }
}

fn formatter_options() -> FormatterTerminalOptions {
    let screen = FormatterScreenExtra {
        size: size_of::<FormatterScreenExtra>(),
        ..FormatterScreenExtra::default()
    };
    let extra = FormatterTerminalExtra {
        size: size_of::<FormatterTerminalExtra>(),
        screen,
        ..FormatterTerminalExtra::default()
    };
    FormatterTerminalOptions {
        size: size_of::<FormatterTerminalOptions>(),
        emit: GHOSTTY_FORMATTER_FORMAT_PLAIN,
        unwrap: false,
        trim: true,
        extra,
        selection: ptr::null(),
    }
}

fn check(operation: &'static str, result: i32) -> Result<(), GhosttyError> {
    if result == GHOSTTY_SUCCESS {
        Ok(())
    } else {
        Err(GhosttyError::Operation { operation, result })
    }
}
