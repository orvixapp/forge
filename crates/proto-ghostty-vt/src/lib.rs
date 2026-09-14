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
const GHOSTTY_INVALID_VALUE: i32 = -2;
const GHOSTTY_FORMATTER_FORMAT_PLAIN: i32 = 0;

type RawTerminal = *mut c_void;
type RawFormatter = *mut c_void;
type RawRenderState = *mut c_void;
type RawRowIterator = *mut c_void;
type RawRowCells = *mut c_void;

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
type RowIteratorNew = unsafe extern "C" fn(*const c_void, *mut RawRowIterator) -> i32;
type RowIteratorFree = unsafe extern "C" fn(RawRowIterator);
type RowIteratorNextDirty = unsafe extern "C" fn(RawRowIterator, *mut u16) -> bool;
type RowGet = unsafe extern "C" fn(RawRowIterator, i32, *mut c_void) -> i32;
type RowCellsNew = unsafe extern "C" fn(*const c_void, *mut RawRowCells) -> i32;
type RowCellsFree = unsafe extern "C" fn(RawRowCells);
type RowCellsNext = unsafe extern "C" fn(RawRowCells) -> bool;
type RowCellsGet = unsafe extern "C" fn(RawRowCells, i32, *mut c_void) -> i32;
type RowIteratorNext = unsafe extern "C" fn(RawRowIterator) -> bool;
type TerminalSet = unsafe extern "C" fn(RawTerminal, i32, *const c_void) -> i32;
type TerminalGet = unsafe extern "C" fn(RawTerminal, i32, *mut c_void) -> i32;
type TerminalScrollViewport = unsafe extern "C" fn(RawTerminal, GhosttyScrollViewport);
type RawKeyEncoder = *mut c_void;
type RawKeyEvent = *mut c_void;
type RawMouseEncoder = *mut c_void;
type RawMouseEvent = *mut c_void;
type KeyEncoderNew = unsafe extern "C" fn(*const c_void, *mut RawKeyEncoder) -> i32;
type KeyEncoderFree = unsafe extern "C" fn(RawKeyEncoder);
type KeyEncoderSetoptFromTerminal = unsafe extern "C" fn(RawKeyEncoder, RawTerminal);
type KeyEncoderEncode =
    unsafe extern "C" fn(RawKeyEncoder, RawKeyEvent, *mut u8, usize, *mut usize) -> i32;
type KeyEventNew = unsafe extern "C" fn(*const c_void, *mut RawKeyEvent) -> i32;
type KeyEventFree = unsafe extern "C" fn(RawKeyEvent);
type KeyEventSetI32 = unsafe extern "C" fn(RawKeyEvent, i32);
type KeyEventSetMods = unsafe extern "C" fn(RawKeyEvent, u16);
type KeyEventSetBool = unsafe extern "C" fn(RawKeyEvent, bool);
type KeyEventSetUtf8 = unsafe extern "C" fn(RawKeyEvent, *const u8, usize);
type KeyEventSetU32 = unsafe extern "C" fn(RawKeyEvent, u32);
type MouseEncoderNew = unsafe extern "C" fn(*const c_void, *mut RawMouseEncoder) -> i32;
type MouseEncoderFree = unsafe extern "C" fn(RawMouseEncoder);
type MouseEncoderSetopt = unsafe extern "C" fn(RawMouseEncoder, i32, *const c_void);
type MouseEncoderSetoptFromTerminal = unsafe extern "C" fn(RawMouseEncoder, RawTerminal);
type MouseEncoderEncode =
    unsafe extern "C" fn(RawMouseEncoder, RawMouseEvent, *mut u8, usize, *mut usize) -> i32;
type MouseEventNew = unsafe extern "C" fn(*const c_void, *mut RawMouseEvent) -> i32;
type MouseEventFree = unsafe extern "C" fn(RawMouseEvent);
type MouseEventSetI32 = unsafe extern "C" fn(RawMouseEvent, i32);
type MouseEventClearButton = unsafe extern "C" fn(RawMouseEvent);
type MouseEventSetMods = unsafe extern "C" fn(RawMouseEvent, u16);
type MouseEventSetPosition = unsafe extern "C" fn(RawMouseEvent, GhosttyMousePosition);
type PasteEncode = unsafe extern "C" fn(*mut u8, usize, bool, *mut u8, usize, *mut usize) -> i32;
type WritePtyFn = unsafe extern "C" fn(RawTerminal, *mut c_void, *const u8, usize);

const TERMINAL_OPT_USERDATA: i32 = 0;
const TERMINAL_OPT_WRITE_PTY: i32 = 1;
const TERMINAL_OPT_SCROLLBACK_MAX_BYTES: i32 = 27;
const TERMINAL_OPT_SCROLLBACK_MAX_LINES: i32 = 28;
const TERMINAL_DATA_ACTIVE_SCREEN: i32 = 6;
const TERMINAL_DATA_SCROLLBAR: i32 = 9;
const TERMINAL_DATA_MOUSE_TRACKING: i32 = 11;
const TERMINAL_DATA_TITLE: i32 = 12;
const TERMINAL_DATA_PWD: i32 = 13;
const TERMINAL_DATA_MODE: i32 = 37;
const MOUSE_ENCODER_OPT_SIZE: i32 = 2;
const SCROLL_VIEWPORT_TOP: i32 = 0;
const SCROLL_VIEWPORT_BOTTOM: i32 = 1;
const SCROLL_VIEWPORT_DELTA: i32 = 2;
const SCROLL_VIEWPORT_ROW: i32 = 3;
/// DEC private mode 2004.
const MODE_BRACKETED_PASTE: u16 = 2004;

#[repr(C)]
#[derive(Clone, Copy)]
union GhosttyScrollViewportValue {
    delta: isize,
    row: usize,
    padding: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GhosttyScrollViewport {
    tag: i32,
    value: GhosttyScrollViewportValue,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GhosttyTerminalScrollbar {
    total: u64,
    offset: u64,
    len: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GhosttyString {
    ptr: *const u8,
    len: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GhosttyTerminalModeConfig {
    mode: u16,
    value: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GhosttyMousePosition {
    x: f32,
    y: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GhosttyMouseEncoderSize {
    size: usize,
    screen_width: u32,
    screen_height: u32,
    cell_width: u32,
    cell_height: u32,
    padding_top: u32,
    padding_bottom: u32,
    padding_right: u32,
    padding_left: u32,
}

#[repr(C)]
struct GhosttyBuffer {
    ptr: *mut u8,
    cap: usize,
    len: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GhosttyColorRgb {
    r: u8,
    g: u8,
    b: u8,
}

#[repr(C)]
struct GhosttyRenderCursor {
    size: usize,
    viewport_has_value: bool,
    viewport_x: u16,
    viewport_y: u16,
    wide_tail: bool,
    visible: bool,
    blinking: bool,
    password_input: bool,
    visual_style: i32,
}

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
    row_iterator_new: RowIteratorNew,
    row_iterator_free: RowIteratorFree,
    row_iterator_next_dirty: RowIteratorNextDirty,
    row_get: RowGet,
    row_cells_new: RowCellsNew,
    row_cells_free: RowCellsFree,
    row_cells_next: RowCellsNext,
    row_cells_get: RowCellsGet,
    row_iterator_next: RowIteratorNext,
    terminal_set: TerminalSet,
    terminal_get: TerminalGet,
    terminal_scroll_viewport: TerminalScrollViewport,
    key_encoder_new: KeyEncoderNew,
    key_encoder_free: KeyEncoderFree,
    key_encoder_setopt_from_terminal: KeyEncoderSetoptFromTerminal,
    key_encoder_encode: KeyEncoderEncode,
    key_event_new: KeyEventNew,
    key_event_free: KeyEventFree,
    key_event_set_action: KeyEventSetI32,
    key_event_set_key: KeyEventSetI32,
    key_event_set_mods: KeyEventSetMods,
    key_event_set_consumed_mods: KeyEventSetMods,
    key_event_set_composing: KeyEventSetBool,
    key_event_set_utf8: KeyEventSetUtf8,
    key_event_set_unshifted_codepoint: KeyEventSetU32,
    mouse_encoder_new: MouseEncoderNew,
    mouse_encoder_free: MouseEncoderFree,
    mouse_encoder_setopt: MouseEncoderSetopt,
    mouse_encoder_setopt_from_terminal: MouseEncoderSetoptFromTerminal,
    mouse_encoder_encode: MouseEncoderEncode,
    mouse_event_new: MouseEventNew,
    mouse_event_free: MouseEventFree,
    mouse_event_set_action: MouseEventSetI32,
    mouse_event_set_button: MouseEventSetI32,
    mouse_event_clear_button: MouseEventClearButton,
    mouse_event_set_mods: MouseEventSetMods,
    mouse_event_set_position: MouseEventSetPosition,
    paste_encode: PasteEncode,
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
            row_iterator_new: symbol!(b"ghostty_render_state_row_iterator_new\0", RowIteratorNew),
            row_iterator_free: symbol!(
                b"ghostty_render_state_row_iterator_free\0",
                RowIteratorFree
            ),
            row_iterator_next_dirty: symbol!(
                b"ghostty_render_state_row_iterator_next_dirty\0",
                RowIteratorNextDirty
            ),
            row_get: symbol!(b"ghostty_render_state_row_get\0", RowGet),
            row_cells_new: symbol!(b"ghostty_render_state_row_cells_new\0", RowCellsNew),
            row_cells_free: symbol!(b"ghostty_render_state_row_cells_free\0", RowCellsFree),
            row_cells_next: symbol!(b"ghostty_render_state_row_cells_next\0", RowCellsNext),
            row_cells_get: symbol!(b"ghostty_render_state_row_cells_get\0", RowCellsGet),
            row_iterator_next: symbol!(
                b"ghostty_render_state_row_iterator_next\0",
                RowIteratorNext
            ),
            terminal_set: symbol!(b"ghostty_terminal_set\0", TerminalSet),
            terminal_get: symbol!(b"ghostty_terminal_get\0", TerminalGet),
            terminal_scroll_viewport: symbol!(
                b"ghostty_terminal_scroll_viewport\0",
                TerminalScrollViewport
            ),
            key_encoder_new: symbol!(b"ghostty_key_encoder_new\0", KeyEncoderNew),
            key_encoder_free: symbol!(b"ghostty_key_encoder_free\0", KeyEncoderFree),
            key_encoder_setopt_from_terminal: symbol!(
                b"ghostty_key_encoder_setopt_from_terminal\0",
                KeyEncoderSetoptFromTerminal
            ),
            key_encoder_encode: symbol!(b"ghostty_key_encoder_encode\0", KeyEncoderEncode),
            key_event_new: symbol!(b"ghostty_key_event_new\0", KeyEventNew),
            key_event_free: symbol!(b"ghostty_key_event_free\0", KeyEventFree),
            key_event_set_action: symbol!(b"ghostty_key_event_set_action\0", KeyEventSetI32),
            key_event_set_key: symbol!(b"ghostty_key_event_set_key\0", KeyEventSetI32),
            key_event_set_mods: symbol!(b"ghostty_key_event_set_mods\0", KeyEventSetMods),
            key_event_set_consumed_mods: symbol!(
                b"ghostty_key_event_set_consumed_mods\0",
                KeyEventSetMods
            ),
            key_event_set_composing: symbol!(b"ghostty_key_event_set_composing\0", KeyEventSetBool),
            key_event_set_utf8: symbol!(b"ghostty_key_event_set_utf8\0", KeyEventSetUtf8),
            key_event_set_unshifted_codepoint: symbol!(
                b"ghostty_key_event_set_unshifted_codepoint\0",
                KeyEventSetU32
            ),
            mouse_encoder_new: symbol!(b"ghostty_mouse_encoder_new\0", MouseEncoderNew),
            mouse_encoder_free: symbol!(b"ghostty_mouse_encoder_free\0", MouseEncoderFree),
            mouse_encoder_setopt: symbol!(b"ghostty_mouse_encoder_setopt\0", MouseEncoderSetopt),
            mouse_encoder_setopt_from_terminal: symbol!(
                b"ghostty_mouse_encoder_setopt_from_terminal\0",
                MouseEncoderSetoptFromTerminal
            ),
            mouse_encoder_encode: symbol!(b"ghostty_mouse_encoder_encode\0", MouseEncoderEncode),
            mouse_event_new: symbol!(b"ghostty_mouse_event_new\0", MouseEventNew),
            mouse_event_free: symbol!(b"ghostty_mouse_event_free\0", MouseEventFree),
            mouse_event_set_action: symbol!(b"ghostty_mouse_event_set_action\0", MouseEventSetI32),
            mouse_event_set_button: symbol!(b"ghostty_mouse_event_set_button\0", MouseEventSetI32),
            mouse_event_clear_button: symbol!(
                b"ghostty_mouse_event_clear_button\0",
                MouseEventClearButton
            ),
            mouse_event_set_mods: symbol!(b"ghostty_mouse_event_set_mods\0", MouseEventSetMods),
            mouse_event_set_position: symbol!(
                b"ghostty_mouse_event_set_position\0",
                MouseEventSetPosition
            ),
            paste_encode: symbol!(b"ghostty_paste_encode\0", PasteEncode),
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
        let result = unsafe { (self.api.render_state_new)(ptr::null(), &raw mut render_state) };
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
            write_pty: None,
        })
    }

    /// Creates a key encoder; configure it from a terminal before encoding.
    ///
    /// # Errors
    ///
    /// Allocation failure inside Ghostty.
    pub fn key_encoder(&self) -> Result<KeyEncoder, GhosttyError> {
        let mut encoder = ptr::null_mut();
        // SAFETY: valid out pointer; null selects Ghostty's allocator.
        let result = unsafe { (self.api.key_encoder_new)(ptr::null(), &raw mut encoder) };
        check("key_encoder_new", result)?;
        let mut event = ptr::null_mut();
        // SAFETY: as above.
        let result = unsafe { (self.api.key_event_new)(ptr::null(), &raw mut event) };
        if let Err(error) = check("key_event_new", result) {
            // SAFETY: the encoder was created and is uniquely owned.
            unsafe { (self.api.key_encoder_free)(encoder) };
            return Err(error);
        }
        if encoder.is_null() || event.is_null() {
            return Err(GhosttyError::NullHandle("key_encoder_new"));
        }
        Ok(KeyEncoder {
            api: Arc::clone(&self.api),
            encoder,
            event,
        })
    }

    /// Creates a mouse encoder; configure it from a terminal before encoding.
    ///
    /// # Errors
    ///
    /// Allocation failure inside Ghostty.
    pub fn mouse_encoder(&self) -> Result<MouseEncoder, GhosttyError> {
        let mut encoder = ptr::null_mut();
        // SAFETY: valid out pointer; null selects Ghostty's allocator.
        let result = unsafe { (self.api.mouse_encoder_new)(ptr::null(), &raw mut encoder) };
        check("mouse_encoder_new", result)?;
        let mut event = ptr::null_mut();
        // SAFETY: as above.
        let result = unsafe { (self.api.mouse_event_new)(ptr::null(), &raw mut event) };
        if let Err(error) = check("mouse_event_new", result) {
            // SAFETY: the encoder was created and is uniquely owned.
            unsafe { (self.api.mouse_encoder_free)(encoder) };
            return Err(error);
        }
        if encoder.is_null() || event.is_null() {
            return Err(GhosttyError::NullHandle("mouse_encoder_new"));
        }
        Ok(MouseEncoder {
            api: Arc::clone(&self.api),
            encoder,
            event,
        })
    }
}

/// Encodes key presses into the byte sequences the application expects.
pub struct KeyEncoder {
    api: Arc<Api>,
    encoder: RawKeyEncoder,
    event: RawKeyEvent,
}

// SAFETY: handles have no thread affinity; the daemon guards them by a mutex.
unsafe impl Send for KeyEncoder {}

impl KeyEncoder {
    /// Encodes `input` with the modes currently active in `terminal`.
    ///
    /// # Errors
    ///
    /// Ghostty rejecting the event.
    pub fn encode(
        &mut self,
        terminal: &GhosttyTerminal,
        input: &KeyInput,
    ) -> Result<Vec<u8>, GhosttyError> {
        // SAFETY: all handles are valid and uniquely owned; the utf8 slice
        // outlives the encode call.
        unsafe {
            (self.api.key_encoder_setopt_from_terminal)(self.encoder, terminal.raw);
            (self.api.key_event_set_action)(self.event, input.action);
            (self.api.key_event_set_key)(self.event, input.key);
            (self.api.key_event_set_mods)(self.event, input.mods);
            (self.api.key_event_set_consumed_mods)(self.event, 0);
            (self.api.key_event_set_composing)(self.event, false);
            let text = input.text.as_deref().unwrap_or("");
            (self.api.key_event_set_utf8)(self.event, text.as_ptr(), text.len());
            (self.api.key_event_set_unshifted_codepoint)(self.event, input.unshifted_codepoint);
        }
        let mut buffer = vec![0_u8; 64];
        let mut written = 0;
        // SAFETY: the buffer exposes exactly the capacity passed to Ghostty.
        let result = unsafe {
            (self.api.key_encoder_encode)(
                self.encoder,
                self.event,
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut written,
            )
        };
        if result == GHOSTTY_OUT_OF_SPACE {
            buffer.resize(written, 0);
            // SAFETY: as above, with the size Ghostty asked for.
            let result = unsafe {
                (self.api.key_encoder_encode)(
                    self.encoder,
                    self.event,
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &raw mut written,
                )
            };
            check("key_encoder_encode", result)?;
        } else {
            check("key_encoder_encode", result)?;
        }
        buffer.truncate(written);
        Ok(buffer)
    }
}

impl Drop for KeyEncoder {
    fn drop(&mut self) {
        // SAFETY: both handles are uniquely owned and freed exactly once.
        unsafe {
            (self.api.key_event_free)(self.event);
            (self.api.key_encoder_free)(self.encoder);
        }
    }
}

/// Encodes mouse events for applications that enabled mouse tracking.
pub struct MouseEncoder {
    api: Arc<Api>,
    encoder: RawMouseEncoder,
    event: RawMouseEvent,
}

// SAFETY: handles have no thread affinity; the daemon guards them by a mutex.
unsafe impl Send for MouseEncoder {}

impl MouseEncoder {
    /// Encodes `input` with the tracking mode and format active in
    /// `terminal`. Positions are cells, so the encoder is told the screen
    /// is `cols`×`rows` "pixels" with 1×1 cells.
    ///
    /// # Errors
    ///
    /// Ghostty rejecting the event.
    pub fn encode(
        &mut self,
        terminal: &GhosttyTerminal,
        cols: u16,
        rows: u16,
        input: MouseInput,
    ) -> Result<Vec<u8>, GhosttyError> {
        let size = GhosttyMouseEncoderSize {
            size: size_of::<GhosttyMouseEncoderSize>(),
            screen_width: u32::from(cols.max(1)),
            screen_height: u32::from(rows.max(1)),
            cell_width: 1,
            cell_height: 1,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        // SAFETY: all handles are valid; `size` lives for the setopt call.
        unsafe {
            (self.api.mouse_encoder_setopt_from_terminal)(self.encoder, terminal.raw);
            (self.api.mouse_encoder_setopt)(
                self.encoder,
                MOUSE_ENCODER_OPT_SIZE,
                (&raw const size).cast(),
            );
            (self.api.mouse_event_set_action)(self.event, input.action);
            match input.button {
                Some(button) => (self.api.mouse_event_set_button)(self.event, button),
                None => (self.api.mouse_event_clear_button)(self.event),
            }
            (self.api.mouse_event_set_mods)(self.event, input.mods);
            (self.api.mouse_event_set_position)(
                self.event,
                GhosttyMousePosition {
                    x: f32::from(input.col) + 0.5,
                    y: f32::from(input.row) + 0.5,
                },
            );
        }
        let mut buffer = vec![0_u8; 64];
        let mut written = 0;
        // SAFETY: the buffer exposes exactly the capacity passed to Ghostty.
        let result = unsafe {
            (self.api.mouse_encoder_encode)(
                self.encoder,
                self.event,
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut written,
            )
        };
        check("mouse_encoder_encode", result)?;
        buffer.truncate(written);
        Ok(buffer)
    }
}

impl Drop for MouseEncoder {
    fn drop(&mut self) {
        // SAFETY: both handles are uniquely owned and freed exactly once.
        unsafe {
            (self.api.mouse_event_free)(self.event);
            (self.api.mouse_encoder_free)(self.encoder);
        }
    }
}

pub struct GhosttyTerminal {
    api: Arc<Api>,
    raw: RawTerminal,
    render_state: RawRenderState,
    /// Owner of the `write_pty` closure handed to Ghostty as userdata.
    write_pty: Option<Box<WritePtyCallback>>,
}

type WritePtyCallback = Box<dyn Fn(&[u8]) + Send>;

/// Trampoline for `GHOSTTY_TERMINAL_OPT_WRITE_PTY`.
unsafe extern "C" fn write_pty_trampoline(
    _terminal: RawTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    if userdata.is_null() || data.is_null() {
        return;
    }
    // SAFETY: userdata is the `*mut WritePtyCallback` installed by
    // `set_write_pty`, alive as long as the terminal; Ghostty guarantees
    // `data` points to `len` readable bytes for the duration of the call.
    let callback = unsafe { &*userdata.cast::<WritePtyCallback>() };
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    callback(bytes);
}

/// Where the viewport should move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollViewport {
    Top,
    Bottom,
    /// Rows; negative is up into the scrollback.
    Delta(i64),
    Row(u64),
}

/// Scrollable area in rows: `offset` is the first visible row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Scrollbar {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

/// Slow-changing terminal state an embedder mirrors in its UI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionState {
    pub title: String,
    pub pwd: String,
    pub mouse_tracking: bool,
    pub alternate_screen: bool,
}

/// Key press described with libghostty-vt's own key codes (see `key/event.h`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInput {
    /// `GHOSTTY_KEY_ACTION_*`: 0 release, 1 press, 2 repeat.
    pub action: i32,
    /// `GHOSTTY_KEY_*` code; 0 is unidentified.
    pub key: i32,
    /// `GHOSTTY_MODS_*` bitmask.
    pub mods: u16,
    pub text: Option<String>,
    pub unshifted_codepoint: u32,
}

/// Mouse event in cell coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseInput {
    /// `GHOSTTY_MOUSE_ACTION_*`: 0 press, 1 release, 2 motion.
    pub action: i32,
    /// `GHOSTTY_MOUSE_BUTTON_*`; `None` for motion without a button.
    pub button: Option<i32>,
    pub mods: u16,
    pub col: u16,
    pub row: u16,
}

// SAFETY: libghostty-vt terminal handles have no thread affinity. Forge never
// accesses a handle concurrently; callers must move it or protect it by a mutex.
unsafe impl Send for GhosttyTerminal {}

impl GhosttyTerminal {
    /// Sets the maximum memory retained by Ghostty for scrollback. This limit
    /// is enforced together with the line limit; whichever is reached first
    /// triggers pruning.
    ///
    /// # Errors
    ///
    /// Returns an error when libghostty rejects the option.
    pub fn set_scrollback_max_bytes(&mut self, bytes: usize) -> Result<(), GhosttyError> {
        // SAFETY: Ghostty reads a `size_t` synchronously during this call.
        let result = unsafe {
            (self.api.terminal_set)(
                self.raw,
                TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                (&raw const bytes).cast(),
            )
        };
        check("terminal_set(scrollback_max_bytes)", result)
    }

    /// Sets the maximum number of physical lines retained in scrollback.
    /// Ghostty prunes at page granularity, so the actual count can be slightly
    /// higher than this limit.
    ///
    /// # Errors
    ///
    /// Returns an error when libghostty rejects the option.
    pub fn set_scrollback_max_lines(&mut self, lines: usize) -> Result<(), GhosttyError> {
        // SAFETY: Ghostty reads a `size_t` synchronously during this call.
        let result = unsafe {
            (self.api.terminal_set)(
                self.raw,
                TERMINAL_OPT_SCROLLBACK_MAX_LINES,
                (&raw const lines).cast(),
            )
        };
        check("terminal_set(scrollback_max_lines)", result)
    }

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

    /// Installs the callback Ghostty uses to answer queries from the
    /// application (device attributes, cursor position reports, mode
    /// reports). Without it those applications wait forever.
    ///
    /// # Errors
    ///
    /// Ghostty rejecting the option.
    pub fn set_write_pty(
        &mut self,
        callback: impl Fn(&[u8]) + Send + 'static,
    ) -> Result<(), GhosttyError> {
        let boxed: Box<WritePtyCallback> = Box::new(Box::new(callback));
        let userdata = Box::into_raw(boxed);
        // SAFETY: pointer options are passed directly; `userdata` stays alive
        // in `self.write_pty` until the terminal is dropped.
        let result = unsafe {
            (self.api.terminal_set)(
                self.raw,
                TERMINAL_OPT_USERDATA,
                userdata.cast_const().cast(),
            )
        };
        if let Err(error) = check("terminal_set(userdata)", result) {
            // SAFETY: reclaiming the box we just leaked.
            drop(unsafe { Box::from_raw(userdata) });
            return Err(error);
        }
        let trampoline: WritePtyFn = write_pty_trampoline;
        // SAFETY: function pointers are passed directly as the option value.
        let result = unsafe {
            (self.api.terminal_set)(
                self.raw,
                TERMINAL_OPT_WRITE_PTY,
                trampoline as *const c_void,
            )
        };
        check("terminal_set(write_pty)", result)?;
        // SAFETY: `userdata` came from `Box::into_raw` above.
        self.write_pty = Some(unsafe { Box::from_raw(userdata) });
        Ok(())
    }

    /// Moves the viewport over the scrollback.
    pub fn scroll_viewport(&mut self, scroll: ScrollViewport) {
        let behavior = match scroll {
            ScrollViewport::Top => GhosttyScrollViewport {
                tag: SCROLL_VIEWPORT_TOP,
                value: GhosttyScrollViewportValue { padding: [0; 2] },
            },
            ScrollViewport::Bottom => GhosttyScrollViewport {
                tag: SCROLL_VIEWPORT_BOTTOM,
                value: GhosttyScrollViewportValue { padding: [0; 2] },
            },
            ScrollViewport::Delta(delta) => GhosttyScrollViewport {
                tag: SCROLL_VIEWPORT_DELTA,
                value: GhosttyScrollViewportValue {
                    delta: isize::try_from(delta).unwrap_or(isize::MAX),
                },
            },
            ScrollViewport::Row(row) => GhosttyScrollViewport {
                tag: SCROLL_VIEWPORT_ROW,
                value: GhosttyScrollViewportValue {
                    row: usize::try_from(row).unwrap_or(usize::MAX),
                },
            },
        };
        // SAFETY: the handle is valid and the struct matches the C layout.
        unsafe { (self.api.terminal_scroll_viewport)(self.raw, behavior) };
    }

    /// Position of the viewport inside the scrollable area.
    ///
    /// # Errors
    ///
    /// Ghostty failing the query.
    pub fn scrollbar(&self) -> Result<Scrollbar, GhosttyError> {
        let value: GhosttyTerminalScrollbar =
            self.terminal_value(TERMINAL_DATA_SCROLLBAR, "terminal_get(scrollbar)")?;
        Ok(Scrollbar {
            total: value.total,
            offset: value.offset,
            len: value.len,
        })
    }

    /// Title, working directory, mouse tracking and active screen.
    ///
    /// # Errors
    ///
    /// Ghostty failing a query.
    pub fn session_state(&self) -> Result<SessionState, GhosttyError> {
        let mouse_tracking: bool =
            self.terminal_value(TERMINAL_DATA_MOUSE_TRACKING, "terminal_get(mouse tracking)")?;
        let screen: i32 =
            self.terminal_value(TERMINAL_DATA_ACTIVE_SCREEN, "terminal_get(active screen)")?;
        Ok(SessionState {
            title: self.terminal_string(TERMINAL_DATA_TITLE, "terminal_get(title)")?,
            pwd: self.terminal_string(TERMINAL_DATA_PWD, "terminal_get(pwd)")?,
            mouse_tracking,
            alternate_screen: screen == 1,
        })
    }

    /// Whether a DEC private mode is set.
    ///
    /// # Errors
    ///
    /// Ghostty failing the query.
    pub fn dec_mode(&self, mode: u16) -> Result<bool, GhosttyError> {
        let mut config = GhosttyTerminalModeConfig {
            mode: mode & 0x7fff,
            value: false,
        };
        // SAFETY: the struct has the documented frozen layout.
        let result = unsafe {
            (self.api.terminal_get)(self.raw, TERMINAL_DATA_MODE, (&raw mut config).cast())
        };
        check("terminal_get(mode)", result)?;
        Ok(config.value)
    }

    /// Bytes to write to the PTY for pasting `text`, bracketed when the
    /// application enabled mode 2004.
    ///
    /// # Errors
    ///
    /// Ghostty failing to encode.
    pub fn encode_paste(&self, text: &str) -> Result<Vec<u8>, GhosttyError> {
        let bracketed = self.dec_mode(MODE_BRACKETED_PASTE)?;
        let mut data = text.as_bytes().to_vec();
        let mut buffer = vec![0_u8; data.len() + 16];
        let mut written = 0;
        // SAFETY: both buffers expose exactly the capacities passed.
        let result = unsafe {
            (self.api.paste_encode)(
                data.as_mut_ptr(),
                data.len(),
                bracketed,
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut written,
            )
        };
        if result == GHOSTTY_OUT_OF_SPACE {
            buffer.resize(written, 0);
            // SAFETY: as above, with the size Ghostty asked for.
            let result = unsafe {
                (self.api.paste_encode)(
                    data.as_mut_ptr(),
                    data.len(),
                    bracketed,
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &raw mut written,
                )
            };
            check("paste_encode", result)?;
        } else {
            check("paste_encode", result)?;
        }
        buffer.truncate(written);
        Ok(buffer)
    }

    fn terminal_value<T: Default>(
        &self,
        data: i32,
        operation: &'static str,
    ) -> Result<T, GhosttyError> {
        let mut value = T::default();
        // SAFETY: each call site pairs the data tag with its documented type.
        let result =
            unsafe { (self.api.terminal_get)(self.raw, data, (&raw mut value).cast::<c_void>()) };
        check(operation, result)?;
        Ok(value)
    }

    fn terminal_string(&self, data: i32, operation: &'static str) -> Result<String, GhosttyError> {
        let mut value = GhosttyString {
            ptr: ptr::null(),
            len: 0,
        };
        // SAFETY: the output is a borrowed string valid until the next
        // mutating call; it is copied immediately.
        let result =
            unsafe { (self.api.terminal_get)(self.raw, data, (&raw mut value).cast::<c_void>()) };
        check(operation, result)?;
        if value.ptr.is_null() || value.len == 0 {
            return Ok(String::new());
        }
        // SAFETY: Ghostty guarantees `len` readable bytes at `ptr`.
        let bytes = unsafe { std::slice::from_raw_parts(value.ptr, value.len) };
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    /// Every row of the viewport, for a client that attaches to a running
    /// session and has no previous frame to patch.
    ///
    /// # Errors
    ///
    /// Ghostty failing to read its render state.
    pub fn full_snapshot(&mut self) -> Result<RenderSnapshot, GhosttyError> {
        // SAFETY: both handles are uniquely owned by this value and valid.
        let result = unsafe { (self.api.render_state_update)(self.render_state, self.raw) };
        check("render_state_update", result)?;
        let cols = self.render_value::<u16>(1, "render_state_get(cols)")?;
        let rows = self.render_value::<u16>(2, "render_state_get(rows)")?;
        let cursor = self.render_cursor()?;
        let dirty_rows = self.read_rows(false)?;
        // SAFETY: the state is valid and the complete frame was read.
        let result = unsafe { (self.api.render_state_clean)(self.render_state) };
        check("render_state_clean", result)?;
        Ok(RenderSnapshot {
            cols,
            rows,
            dirty: DirtyState::Full,
            dirty_rows,
            cursor,
        })
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
        let cursor = self.render_cursor()?;
        let dirty_rows = if dirty == DirtyState::Clean {
            Vec::new()
        } else {
            self.read_rows(dirty != DirtyState::Full)?
        };
        // SAFETY: the state is valid and the complete prototype frame was read.
        let result = unsafe { (self.api.render_state_clean)(self.render_state) };
        check("render_state_clean", result)?;
        Ok(RenderSnapshot {
            cols,
            rows,
            dirty,
            dirty_rows,
            cursor,
        })
    }

    fn render_cursor(&self) -> Result<Option<RenderCursor>, GhosttyError> {
        let mut cursor = GhosttyRenderCursor {
            size: size_of::<GhosttyRenderCursor>(),
            viewport_has_value: false,
            viewport_x: 0,
            viewport_y: 0,
            wide_tail: false,
            visible: false,
            blinking: false,
            password_input: false,
            visual_style: 0,
        };
        let result =
            unsafe { (self.api.render_state_get)(self.render_state, 18, (&raw mut cursor).cast()) };
        check("render_state_get(cursor)", result)?;
        if !cursor.viewport_has_value {
            return Ok(None);
        }
        Ok(Some(RenderCursor {
            x: cursor.viewport_x,
            y: cursor.viewport_y,
            visible: cursor.visible,
            blinking: cursor.blinking,
            style: CursorStyle::try_from(cursor.visual_style)?,
        }))
    }

    /// Rows of the viewport: only the dirty ones, or every row.
    fn read_rows(&self, only_dirty: bool) -> Result<Vec<RenderRow>, GhosttyError> {
        let mut iterator = ptr::null_mut();
        let result = unsafe { (self.api.row_iterator_new)(ptr::null(), &raw mut iterator) };
        check("render_state_row_iterator_new", result)?;
        if iterator.is_null() {
            return Err(GhosttyError::NullHandle("render_state_row_iterator_new"));
        }
        let mut iterator = RowIteratorGuard {
            api: Arc::clone(&self.api),
            raw: iterator,
        };
        let result = unsafe {
            (self.api.render_state_get)(self.render_state, 4, (&raw mut iterator.raw).cast())
        };
        check("render_state_get(row iterator)", result)?;

        let mut cells = ptr::null_mut();
        let result = unsafe { (self.api.row_cells_new)(ptr::null(), &raw mut cells) };
        check("render_state_row_cells_new", result)?;
        if cells.is_null() {
            return Err(GhosttyError::NullHandle("render_state_row_cells_new"));
        }
        let mut cells = RowCellsGuard {
            api: Arc::clone(&self.api),
            raw: cells,
        };
        let mut rows = Vec::new();
        let mut viewport_y = 0_u16;
        loop {
            let mut y = 0;
            let more = if only_dirty {
                unsafe { (self.api.row_iterator_next_dirty)(iterator.raw, &raw mut y) }
            } else {
                y = viewport_y;
                let advanced = unsafe { (self.api.row_iterator_next)(iterator.raw) };
                if advanced {
                    viewport_y = viewport_y.saturating_add(1);
                }
                advanced
            };
            if !more {
                break;
            }
            let result =
                unsafe { (self.api.row_get)(iterator.raw, 3, (&raw mut cells.raw).cast()) };
            check("render_state_row_get(cells)", result)?;
            let mut row_cells = Vec::new();
            while unsafe { (self.api.row_cells_next)(cells.raw) } {
                row_cells.push(RenderCell {
                    text: self.cell_text(cells.raw)?,
                    foreground: self.cell_color(cells.raw, 6)?,
                    background: self.cell_color(cells.raw, 5)?,
                    styled: self.cell_value::<bool>(cells.raw, 8, "cell has styling")?,
                });
            }
            rows.push(RenderRow {
                y,
                cells: row_cells,
            });
        }
        Ok(rows)
    }

    fn cell_text(&self, cells: RawRowCells) -> Result<String, GhosttyError> {
        let mut buffer = GhosttyBuffer {
            ptr: ptr::null_mut(),
            cap: 0,
            len: 0,
        };
        let query = unsafe { (self.api.row_cells_get)(cells, 9, (&raw mut buffer).cast()) };
        if buffer.len == 0 && query == GHOSTTY_SUCCESS {
            return Ok(String::new());
        }
        if query != GHOSTTY_OUT_OF_SPACE {
            check("render_state_row_cells_get(text size)", query)?;
        }
        let mut bytes = vec![0; buffer.len];
        buffer.ptr = bytes.as_mut_ptr();
        buffer.cap = bytes.len();
        let result = unsafe { (self.api.row_cells_get)(cells, 9, (&raw mut buffer).cast()) };
        check("render_state_row_cells_get(text)", result)?;
        bytes.truncate(buffer.len);
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn cell_color(&self, cells: RawRowCells, data: i32) -> Result<Option<Rgb>, GhosttyError> {
        let mut color = GhosttyColorRgb::default();
        let result = unsafe { (self.api.row_cells_get)(cells, data, (&raw mut color).cast()) };
        if result == GHOSTTY_INVALID_VALUE {
            return Ok(None);
        }
        check("render_state_row_cells_get(color)", result)?;
        Ok(Some(Rgb {
            r: color.r,
            g: color.g,
            b: color.b,
        }))
    }

    fn cell_value<T: Default>(
        &self,
        cells: RawRowCells,
        data: i32,
        operation: &'static str,
    ) -> Result<T, GhosttyError> {
        let mut value = T::default();
        let result = unsafe { (self.api.row_cells_get)(cells, data, (&raw mut value).cast()) };
        check(operation, result)?;
        Ok(value)
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
            (self.api.render_state_get)(self.render_state, data, (&raw mut value).cast::<c_void>())
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
    pub dirty_rows: Vec<RenderRow>,
    pub cursor: Option<RenderCursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderCursor {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
    pub blinking: bool,
    pub style: CursorStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorStyle {
    Bar,
    Block,
    Underline,
    HollowBlock,
}

impl TryFrom<i32> for CursorStyle {
    type Error = GhosttyError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Bar),
            1 => Ok(Self::Block),
            2 => Ok(Self::Underline),
            3 => Ok(Self::HollowBlock),
            result => Err(GhosttyError::Operation {
                operation: "render_state_get(cursor style)",
                result,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderRow {
    pub y: u16,
    pub cells: Vec<RenderCell>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderCell {
    pub text: String,
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub styled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

struct RowIteratorGuard {
    api: Arc<Api>,
    raw: RawRowIterator,
}
impl Drop for RowIteratorGuard {
    fn drop(&mut self) {
        unsafe { (self.api.row_iterator_free)(self.raw) };
    }
}

struct RowCellsGuard {
    api: Arc<Api>,
    raw: RawRowCells,
}
impl Drop for RowCellsGuard {
    fn drop(&mut self) {
        unsafe { (self.api.row_cells_free)(self.raw) };
    }
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
