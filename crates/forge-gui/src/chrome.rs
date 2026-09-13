//! Rendering of the window chrome: top bar with tabs, window controls and
//! resize handles, the pane tree, the status lines, notifications, the
//! command palette and the process explorer.

use crate::{
    grid_element::{TerminalGridElement, color},
    window::{ForgeWindow, NotificationLevel},
};
use forge_gui::{
    config::Language,
    shell::{PaneTree, Rect, ShellCommand, search_commands},
};
use gpui::{
    AnyElement, Context, CursorStyle, ImageSource, MouseButton, ResizeEdge, Resource, SharedString,
    Window, div, img, prelude::*, px, rgb,
};

pub const TOPBAR_HEIGHT: f32 = 36.0;
pub const STATUS_HEIGHT: f32 = 20.0;
/// Width of the invisible strips along the window edges that start a
/// native resize when the compositor leaves decorations to us.
const RESIZE_INSET: f32 = 6.0;

pub fn render_window(
    view: &mut ForgeWindow,
    window: &mut Window,
    cx: &mut Context<ForgeWindow>,
) -> AnyElement {
    let theme = view.theme;
    let tree = view.visible_tree();
    window.set_client_inset(px(RESIZE_INSET));
    div()
        .id("forge-window")
        .track_focus(&view.focus)
        .on_key_down(cx.listener(ForgeWindow::on_key_down))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, event, _, cx| view.on_mouse_down(event, cx)),
        )
        .on_mouse_move(cx.listener(|view, event, _, cx| view.on_mouse_move(event, cx)))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|view, event, _, cx| view.on_mouse_up(event, cx)),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|view, event, _, cx| view.on_mouse_up(event, cx)),
        )
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(color(theme.background))
        .text_color(color(theme.foreground))
        .font_family(view.config.font.family.clone())
        .child(topbar(view, cx))
        .child(panes(view, &tree, window, cx))
        .when(view.process_explorer, |root| {
            root.child(process_explorer(view, cx))
        })
        .when(view.palette.open, |root| root.child(palette(view)))
        .children(resize_handles())
        .into_any_element()
}

fn topbar(view: &ForgeWindow, cx: &mut Context<ForgeWindow>) -> impl IntoElement {
    let theme = view.theme;
    div()
        .h(px(TOPBAR_HEIGHT))
        .w_full()
        .flex()
        .items_center()
        .px(px(10.0))
        .gap(px(8.0))
        .bg(color(theme.chrome))
        .border_b_1()
        .border_color(color(theme.chrome_border))
        // `img("name.svg")` would parse the name as a URI and try HTTP.
        .child(
            img(ImageSource::Resource(Resource::Embedded(
                "forge-logo.svg".into(),
            )))
            .size(px(19.0)),
        )
        .children(tab_buttons(view, cx))
        .child(
            chrome_button("new-terminal-tab", "+", theme.chrome_active, view).on_click(
                cx.listener(|view, _, _, cx| {
                    view.create_terminal_tab(None, cx);
                }),
            ),
        )
        .child(
            div()
                .id("forge-drag-region")
                .h_full()
                .flex_1()
                .cursor_default()
                .on_mouse_down(MouseButton::Left, |event, window, _| {
                    if event.click_count >= 2 {
                        window.zoom_window();
                    } else {
                        window.start_window_move();
                    }
                })
                .on_mouse_down(MouseButton::Right, |event, window, _| {
                    window.show_window_menu(event.position);
                }),
        )
        .child(topbar_notice(view))
        .child(
            chrome_button("forge-minimize", "—", theme.chrome_active, view)
                .on_click(|_, window, _| window.minimize_window()),
        )
        .child(
            chrome_button("forge-maximize", "□", theme.chrome_active, view)
                .on_click(|_, window, _| window.zoom_window()),
        )
        .child(
            chrome_button("forge-close", "×", theme.danger, view)
                .on_click(|_, window, _| window.remove_window()),
        )
}

fn tab_buttons(view: &ForgeWindow, cx: &mut Context<ForgeWindow>) -> Vec<AnyElement> {
    let tabs: Vec<(usize, String, u64)> = view
        .tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| (index, tab.title.clone(), tab.id))
        .collect();
    tabs.into_iter()
        .map(|(index, title, id)| tab_button(view, index, &title, id, cx).into_any_element())
        .collect()
}

fn tab_button(
    view: &ForgeWindow,
    index: usize,
    title: &str,
    id: u64,
    cx: &mut Context<ForgeWindow>,
) -> impl IntoElement {
    let theme = view.theme;
    let active = index == view.active_tab;
    div()
        .id(SharedString::from(format!("terminal-tab-{id}")))
        .h(px(28.0))
        .min_w(px(120.0))
        .max_w(px(210.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(6.0))
        .bg(color(if active {
            theme.chrome_active
        } else {
            theme.chrome
        }))
        .border_1()
        .border_color(color(if active {
            theme.chrome_active_border
        } else {
            theme.chrome
        }))
        .text_size(px(12.0))
        .text_color(color(if active {
            theme.foreground
        } else {
            theme.muted
        }))
        .cursor_pointer()
        .hover(move |style| style.bg(color(theme.chrome_active)))
        .on_click(cx.listener(move |view, _, _, cx| view.activate_tab(index, cx)))
        .child(div().flex_1().overflow_hidden().child(title.to_string()))
        .when(active, |tab| {
            tab.child(
                div()
                    .id(SharedString::from(format!("terminal-tab-close-{id}")))
                    .size(px(16.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.0))
                    .hover(move |style| style.bg(color(theme.chrome_active_border)))
                    .on_click(cx.listener(move |view, _, _, cx| view.close_tab(index, cx)))
                    .child("×"),
            )
        })
}

/// Latest notification, or the new-tab hint when there is none.
fn topbar_notice(view: &ForgeWindow) -> impl IntoElement {
    let theme = view.theme;
    let english = view.config.ui.language == Language::English;
    if let Some(note) = view.notifications.last() {
        let tint = match note.level {
            NotificationLevel::Info => theme.accent,
            NotificationLevel::Warning => theme.cursor,
            NotificationLevel::Error => theme.danger,
        };
        return div()
            .text_size(px(12.0))
            .text_color(color(tint))
            .max_w(px(420.0))
            .overflow_hidden()
            .child(note.text.clone());
    }
    let hint = view
        .keymap
        .chord_for(ShellCommand::NewTerminalTab)
        .map(|chord| {
            if english {
                format!("{chord} · new tab")
            } else {
                format!("{chord} · nueva pestaña")
            }
        })
        .unwrap_or_default();
    div()
        .text_size(px(12.0))
        .text_color(color(theme.muted))
        .child(hint)
}

fn chrome_button(
    id: &'static str,
    label: &'static str,
    hover: forge_gui::config::HexColor,
    view: &ForgeWindow,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.0))
        .text_color(color(view.theme.muted))
        .cursor_pointer()
        .hover(move |style| style.bg(color(hover)))
        .child(label)
}

/// Gap between split panes, in pixels.
const PANE_GAP: f32 = 2.0;

/// Panes are absolutely positioned from geometry computed by `PaneTree`:
/// one flat container instead of nested flex boxes, whose layout cost grows
/// with the depth of the split tree.
fn panes(
    view: &ForgeWindow,
    tree: &PaneTree,
    window: &Window,
    cx: &mut Context<ForgeWindow>,
) -> AnyElement {
    let viewport = window.viewport_size();
    let area = Rect {
        x: 0.0,
        y: 0.0,
        width: f32::from(viewport.width),
        height: (f32::from(viewport.height) - TOPBAR_HEIGHT).max(0.0),
    };
    let rects = tree.layout(area, PANE_GAP);
    div()
        .relative()
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .children(
            rects
                .into_iter()
                .map(|(index, rect)| pane_leaf(view, index, rect, cx)),
        )
        .into_any_element()
}

fn pane_leaf(
    view: &ForgeWindow,
    index: usize,
    rect: Rect,
    cx: &mut Context<ForgeWindow>,
) -> AnyElement {
    let theme = view.theme;
    let active = index == view.active_tab;
    let tab = &view.tabs[index];
    let (cols, rows) = tab.terminal.grid.dimensions();
    let status = match &view.marked_text {
        Some(marked) if active => format!("{cols}×{rows} · {} · IME: {marked}", tab.status),
        _ => format!("{cols}×{rows} · {}", tab.status),
    };
    let mut grid = TerminalGridElement::new(cx.entity(), index, ForgeWindow::pane_surface)
        .with_padding(px(view.config.terminal.padding));
    if active {
        grid = grid.with_input_focus(view.focus.clone());
    }
    div()
        .id(("pane", index))
        .absolute()
        .left(px(rect.x))
        .top(px(rect.y))
        .w(px(rect.width))
        .h(px(rect.height))
        .flex()
        .flex_col()
        .overflow_hidden()
        .border_1()
        .border_color(color(if active {
            theme.chrome_active_border
        } else {
            theme.chrome_border
        }))
        .cursor(CursorStyle::IBeam)
        .child(grid)
        // The status line belongs to the focused pane; inactive panes keep
        // every pixel for their grid.
        .when(view.config.terminal.show_status && active, |pane| {
            pane.child(
                div()
                    .h(px(STATUS_HEIGHT))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .text_size(px(11.0))
                    .text_color(color(theme.accent))
                    .bg(color(theme.chrome))
                    .child(status),
            )
        })
        .into_any_element()
}

/// Eight invisible strips over the window edges; each starts a native resize
/// and shows the matching cursor without re-rendering on mouse movement.
fn resize_handles() -> Vec<AnyElement> {
    let inset = px(RESIZE_INSET);
    let corner = px(RESIZE_INSET * 2.0);
    let handle = |id: &'static str, edge: ResizeEdge, cursor: CursorStyle| {
        div().id(id).absolute().cursor(cursor).on_mouse_down(
            MouseButton::Left,
            move |_, window, _| {
                window.start_window_resize(edge);
            },
        )
    };
    vec![
        handle("resize-top", ResizeEdge::Top, CursorStyle::ResizeUpDown)
            .top_0()
            .left(corner)
            .right(corner)
            .h(inset)
            .into_any_element(),
        handle(
            "resize-bottom",
            ResizeEdge::Bottom,
            CursorStyle::ResizeUpDown,
        )
        .bottom_0()
        .left(corner)
        .right(corner)
        .h(inset)
        .into_any_element(),
        handle(
            "resize-left",
            ResizeEdge::Left,
            CursorStyle::ResizeLeftRight,
        )
        .left_0()
        .top(corner)
        .bottom(corner)
        .w(inset)
        .into_any_element(),
        handle(
            "resize-right",
            ResizeEdge::Right,
            CursorStyle::ResizeLeftRight,
        )
        .right_0()
        .top(corner)
        .bottom(corner)
        .w(inset)
        .into_any_element(),
        handle(
            "resize-top-left",
            ResizeEdge::TopLeft,
            CursorStyle::ResizeUpLeftDownRight,
        )
        .top_0()
        .left_0()
        .size(corner)
        .into_any_element(),
        handle(
            "resize-top-right",
            ResizeEdge::TopRight,
            CursorStyle::ResizeUpRightDownLeft,
        )
        .top_0()
        .right_0()
        .size(corner)
        .into_any_element(),
        handle(
            "resize-bottom-left",
            ResizeEdge::BottomLeft,
            CursorStyle::ResizeUpRightDownLeft,
        )
        .bottom_0()
        .left_0()
        .size(corner)
        .into_any_element(),
        handle(
            "resize-bottom-right",
            ResizeEdge::BottomRight,
            CursorStyle::ResizeUpLeftDownRight,
        )
        .bottom_0()
        .right_0()
        .size(corner)
        .into_any_element(),
    ]
}

fn process_explorer(view: &ForgeWindow, cx: &mut Context<ForgeWindow>) -> impl IntoElement {
    let theme = view.theme;
    div()
        .id("process-explorer")
        .absolute()
        .top(px(TOPBAR_HEIGHT + 12.0))
        .left(px(24.0))
        .w(px(460.0))
        .p(px(12.0))
        .rounded(px(8.0))
        .bg(color(theme.chrome))
        .border_1()
        .border_color(color(theme.chrome_border))
        .text_size(px(12.0))
        .child(
            div()
                .flex()
                .justify_between()
                .child(format!(
                    "Forge · PID {} · PSS {} MiB · {} frames",
                    std::process::id(),
                    crate::process_pss_kib().unwrap_or(0) / 1024,
                    view.render_count()
                ))
                .child(
                    div()
                        .id("close-process-explorer")
                        .cursor_pointer()
                        .text_color(color(theme.muted))
                        .child("×")
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.process_explorer = false;
                            cx.notify();
                        })),
                ),
        )
        .child(
            div()
                .mt(px(4.0))
                .text_color(color(theme.muted))
                .child(format!(
                    "tema {} · fuente {} {}px · config {}",
                    view.theme_name,
                    view.config.font.family,
                    view.config.font.size,
                    view.factory
                        .sources
                        .user
                        .as_deref()
                        .map_or_else(|| "—".into(), |path| path.display().to_string())
                )),
        )
        .children(view.tabs.iter().map(|tab| {
            let (cols, rows) = tab.terminal.grid.dimensions();
            div()
                .mt(px(4.0))
                .child(format!("{} · {cols}×{rows} · {}", tab.title, tab.status))
        }))
}

fn palette(view: &ForgeWindow) -> impl IntoElement {
    let theme = view.theme;
    let english = view.config.ui.language == Language::English;
    let matches = search_commands(&view.palette.query);
    let first_visible = view.palette.index.saturating_sub(5);
    div()
        .id("command-palette")
        .absolute()
        .top(px(TOPBAR_HEIGHT + 28.0))
        .left(px(48.0))
        .w(px(440.0))
        .p(px(12.0))
        .rounded(px(8.0))
        .bg(color(theme.chrome))
        .border_1()
        .border_color(color(theme.chrome_border))
        .child(
            div()
                .text_size(px(14.0))
                .text_color(color(theme.foreground))
                .child(format!("› {}", view.palette.query)),
        )
        .child(
            div()
                .mt(px(6.0))
                .text_size(px(11.0))
                .text_color(color(theme.muted))
                .child(if english {
                    "↑↓ select · Enter runs · Esc closes"
                } else {
                    "↑↓ selecciona · Enter ejecuta · Esc cierra"
                }),
        )
        .children(
            matches
                .into_iter()
                .enumerate()
                .skip(first_visible)
                .take(6)
                .map(|(index, item)| {
                    let selected = index == view.palette.index;
                    div()
                        .mt(px(7.0))
                        .px(px(8.0))
                        .py(px(5.0))
                        .rounded(px(4.0))
                        .flex()
                        .justify_between()
                        .bg(color(if selected {
                            theme.highlight
                        } else {
                            theme.chrome_active
                        }))
                        .text_size(px(13.0))
                        .child(item.command.title())
                        .child(
                            div()
                                .text_color(color(theme.muted))
                                .child(view.keymap.chord_for(item.command).unwrap_or_default()),
                        )
                }),
        )
        .when(view.palette.query.is_empty() && !english, |root| {
            root.child(
                div()
                    .mt(px(8.0))
                    .text_size(px(11.0))
                    .text_color(rgb(0x7f_8a_a3))
                    .child("Escribe para filtrar comandos"),
            )
        })
}
