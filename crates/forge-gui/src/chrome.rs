//! Rendering of the window chrome: top bar with tabs, window controls and
//! resize handles, the pane tree, the status lines, notifications, the
//! command palette and the process explorer.

use crate::{
    agent::{MessageRole, TimelineItem, ToolState},
    grid_element::{TerminalGridElement, color},
    window::{ConfirmationKind, ForgeWindow, NotificationLevel, TabContent},
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
        .on_mouse_down(MouseButton::Left, cx.listener(ForgeWindow::on_mouse_down))
        .on_mouse_move(cx.listener(ForgeWindow::on_mouse_move))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|view, event, _, cx| view.on_mouse_up(event, cx)),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|view, event, _, cx| view.on_mouse_up(event, cx)),
        )
        .on_mouse_down(MouseButton::Right, cx.listener(ForgeWindow::on_mouse_down))
        .on_mouse_up(
            MouseButton::Right,
            cx.listener(|view, event, _, cx| view.on_mouse_up(event, cx)),
        )
        .on_mouse_down(MouseButton::Middle, cx.listener(ForgeWindow::on_mouse_down))
        .on_mouse_up(
            MouseButton::Middle,
            cx.listener(|view, event, _, cx| view.on_mouse_up(event, cx)),
        )
        .on_scroll_wheel(cx.listener(|view, event, _, cx| view.on_scroll_wheel(event, cx)))
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
        .when(view.search.open, |root| root.child(search_bar(view, cx)))
        .when(
            view.find.open && view.active_tab().editor().is_some(),
            |root| root.child(find_bar(view)),
        )
        .when(view.finder.is_some(), |root| {
            root.child(finder_overlay(view))
        })
        .when_some(view.context_menu.as_ref(), |root, menu| {
            root.child(context_menu(view, menu, cx))
        })
        .when_some(view.confirmation.as_ref(), |root, confirmation| {
            root.child(confirmation_dialog(view, confirmation))
        })
        .when_some(view.rename.as_ref(), |root, prompt| {
            root.child(text_prompt(view, prompt))
        })
        .when_some(view.picker.as_ref(), |root, picker| {
            root.child(picker_overlay(view, picker))
        })
        .children(resize_handles())
        .into_any_element()
}

fn topbar(view: &ForgeWindow, cx: &mut Context<ForgeWindow>) -> impl IntoElement {
    let theme = view.theme;
    div()
        .h(px(TOPBAR_HEIGHT))
        .w_full()
        .overflow_hidden()
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
        .map(|(index, tab)| (index, tab.title().to_string(), tab.id))
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
        .overflow_hidden()
        .whitespace_nowrap()
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
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .child(title.to_string()),
        )
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
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.request_close_tab(index, window, cx);
                    }))
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
            .whitespace_nowrap()
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
    // Terminals show their viewport and scrollback; editors their cursor.
    let (content, status, scrollbar): (AnyElement, String, Option<AnyElement>) = match &tab.content
    {
        TabContent::Terminal(terminal) => {
            let (cols, rows) = terminal.terminal.grid.dimensions();
            let viewport = terminal.terminal.grid.viewport();
            let scrolled = viewport.scrolled_back();
            let status = match &view.marked_text {
                Some(marked) if active => {
                    format!("{cols}×{rows} · {} · IME: {marked}", terminal.status)
                }
                _ if scrolled => format!(
                    "{cols}×{rows} · scrollback −{} · {}",
                    viewport
                        .total
                        .saturating_sub(viewport.offset + viewport.len),
                    terminal.status
                ),
                _ => format!("{cols}×{rows} · {}", terminal.status),
            };
            let mut grid = TerminalGridElement::new(cx.entity(), index, ForgeWindow::pane_surface)
                .with_padding(px(view.config.terminal.padding));
            if active {
                grid = grid.with_input_focus(view.focus.clone());
            }
            (
                grid.into_any_element(),
                status,
                scrolled.then(|| scrollbar(viewport, rect, theme).into_any_element()),
            )
        }
        TabContent::Editor(editor) => {
            let visible = crate::editor::visible_range(editor);
            let total = editor.buffer.len_lines() as u64;
            let viewport = proto_ipc::Viewport {
                total,
                offset: visible.start as u64,
                len: visible.len() as u64,
            };
            let status = match &view.marked_text {
                Some(marked) if active => format!("{} · IME: {marked}", editor.status()),
                _ => editor.status(),
            };
            let mut element = crate::editor::EditorElement::new(cx.entity(), index);
            if active {
                element = element.with_input_focus(view.focus.clone());
            }
            (
                element.into_any_element(),
                status,
                (total > viewport.len).then(|| scrollbar(viewport, rect, theme).into_any_element()),
            )
        }
        TabContent::Agent(agent) => (
            agent_panel(agent, theme).into_any_element(),
            agent.status.clone(),
            None,
        ),
    };
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
        .child(content)
        .children(scrollbar)
        // The status line belongs to the focused pane; inactive panes keep
        // every pixel for their content.
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

/// Thin indicator at the pane's right edge while the viewport is scrolled
/// back; hidden on the live screen so an idle terminal paints nothing extra.
fn scrollbar(
    viewport: proto_ipc::Viewport,
    rect: Rect,
    theme: forge_gui::theme::ThemeColors,
) -> impl IntoElement {
    #[allow(clippy::cast_precision_loss)]
    let fraction = |rows: u64| rows as f32 / viewport.total.max(1) as f32;
    let track = (rect.height - STATUS_HEIGHT).max(0.0);
    let top = track * fraction(viewport.offset);
    let height = (track * fraction(viewport.len)).max(12.0);
    div()
        .absolute()
        .right(px(2.0))
        .top(px(top))
        .w(px(4.0))
        .h(px(height))
        .rounded(px(2.0))
        .bg(color(theme.muted))
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
            let size = match &tab.content {
                TabContent::Terminal(terminal) => {
                    let (cols, rows) = terminal.terminal.grid.dimensions();
                    format!("{cols}×{rows}")
                }
                TabContent::Editor(editor) => format!("{} líneas", editor.buffer.len_lines()),
                TabContent::Agent(agent) => format!("{} eventos", agent.timeline.len()),
            };
            div()
                .mt(px(4.0))
                .child(format!("{} · {size} · {}", tab.title(), tab.status()))
        }))
}

fn agent_panel(
    agent: &crate::agent::AgentTab,
    theme: forge_gui::theme::ThemeColors,
) -> impl IntoElement {
    let start = agent.timeline.len().saturating_sub(200);
    div()
        .size_full()
        .flex()
        .flex_col()
        .p(px(14.0))
        .gap(px(8.0))
        .overflow_hidden()
        .child(
            div()
                .text_size(px(15.0))
                .text_color(color(theme.accent))
                .child(format!(
                    "Sesión ACP · {} · {}",
                    agent.agent_name,
                    agent.session_id.as_deref().unwrap_or("sin id")
                )),
        )
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .children(
                    agent
                        .visible_timeline(start..agent.timeline.len())
                        .iter()
                        .map(|item| {
                            let (label, body, tint) = match item {
                                TimelineItem::Message { role, text } => {
                                    let label = match role {
                                        MessageRole::User => "Tú",
                                        MessageRole::Agent => "Agente",
                                        MessageRole::System => "Sistema",
                                    };
                                    (label, text.clone(), theme.foreground)
                                }
                                TimelineItem::ToolCall {
                                    title,
                                    state,
                                    detail,
                                    ..
                                } => {
                                    let state = match state {
                                        ToolState::Pending => "pendiente",
                                        ToolState::Running => "en curso",
                                        ToolState::Succeeded => "completada",
                                        ToolState::Failed => "falló",
                                        ToolState::WaitingPermission => "espera permiso",
                                    };
                                    (
                                        "Herramienta",
                                        format!("{title} · {state}\n{detail}"),
                                        theme.accent,
                                    )
                                }
                                TimelineItem::Plan { title, entries } => (
                                    "Plan",
                                    format!("{title}\n{}", entries.join("\n")),
                                    theme.accent,
                                ),
                            };
                            div()
                                .px(px(10.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .bg(color(theme.chrome))
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(color(tint))
                                        .child(label),
                                )
                                .child(div().text_size(px(13.0)).child(body))
                                .into_any_element()
                        }),
                ),
        )
        .child(
            div()
                .min_h(px(42.0))
                .px(px(10.0))
                .py(px(8.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(color(theme.chrome_active_border))
                .child(if agent.prompt.is_empty() {
                    "Escribe un prompt…  (Enter para enviar)".to_owned()
                } else {
                    agent.prompt.clone()
                }),
        )
}

/// Right-click menu at the pointer: command titles with their chords.
fn context_menu(
    view: &ForgeWindow,
    menu: &crate::window::ContextMenu,
    cx: &mut Context<ForgeWindow>,
) -> impl IntoElement {
    let theme = view.theme;
    div()
        .id("context-menu")
        .absolute()
        .left(menu.position.x)
        .top(menu.position.y)
        .w(px(260.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .bg(color(theme.chrome))
        .border_1()
        .border_color(color(theme.chrome_active_border))
        .text_size(px(12.0))
        .children(menu.items.iter().enumerate().map(|(index, command)| {
            let selected = index == menu.index;
            div()
                .id(("context-menu-item", index))
                .px(px(10.0))
                .py(px(4.0))
                .flex()
                .justify_between()
                .cursor_pointer()
                .bg(color(if selected {
                    theme.highlight
                } else {
                    theme.chrome
                }))
                .hover(move |style| style.bg(color(theme.highlight)))
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.context_menu_pick(index, window, cx);
                }))
                .child(command.title())
                .child(
                    div()
                        .text_color(color(theme.muted))
                        .child(view.keymap.chord_for(*command).unwrap_or_default()),
                )
        }))
}

/// Floating box shared by the small overlays: same position as the palette.
fn overlay_box(view: &ForgeWindow, id: &'static str) -> gpui::Stateful<gpui::Div> {
    let theme = view.theme;
    div()
        .id(id)
        .absolute()
        .top(px(TOPBAR_HEIGHT + 28.0))
        .left(px(48.0))
        .w(px(440.0))
        .p(px(12.0))
        .rounded(px(8.0))
        .bg(color(theme.chrome))
        .border_1()
        .border_color(color(theme.chrome_border))
        .flex()
        .flex_col()
        .gap(px(6.0))
}

fn text_prompt(view: &ForgeWindow, prompt: &crate::window::TextPrompt) -> impl IntoElement {
    let theme = view.theme;
    let english = view.config.ui.language == Language::English;
    overlay_box(view, "text-prompt")
        .child(
            div()
                .text_size(px(12.0))
                .text_color(color(theme.muted))
                .child(prompt.title.clone()),
        )
        .child(
            div()
                .text_size(px(14.0))
                .text_color(color(theme.foreground))
                .child(format!("› {}▏", prompt.value)),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(color(theme.muted))
                .child(if english {
                    "Enter applies · empty restores the automatic name · Esc cancels"
                } else {
                    "Enter aplica · vacío recupera el nombre automático · Esc cancela"
                }),
        )
}

fn picker_overlay(view: &ForgeWindow, picker: &crate::window::Picker) -> impl IntoElement {
    let theme = view.theme;
    let english = view.config.ui.language == Language::English;
    overlay_box(view, "picker")
        .child(
            div()
                .text_size(px(14.0))
                .text_color(color(theme.foreground))
                .child(picker.title.clone()),
        )
        .children(picker.items.iter().enumerate().map(|(index, item)| {
            let selected = index == picker.index;
            div()
                .px(px(8.0))
                .py(px(5.0))
                .rounded(px(4.0))
                .bg(color(if selected {
                    theme.highlight
                } else {
                    theme.chrome_active
                }))
                .text_size(px(13.0))
                .child(item.clone())
        }))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(color(theme.muted))
                .child(if english {
                    "↑↓ select · Enter opens · Esc closes"
                } else {
                    "↑↓ selecciona · Enter abre · Esc cierra"
                }),
        )
}

/// File finder / project search: a query line, a status line and the
/// matching rows, with the selected one highlighted.
#[allow(clippy::too_many_lines)]
fn finder_overlay(view: &ForgeWindow) -> AnyElement {
    let theme = view.theme;
    let english = view.config.ui.language == Language::English;
    let Some(finder) = &view.finder else {
        return div().into_any_element();
    };
    let rows = view.finder_rows();
    let first = view.finder_first_row();
    let status = view.finder_status();
    let (title, hint) = match finder.mode {
        crate::project::FinderMode::Files => (
            if english {
                "Go to file"
            } else {
                "Ir a archivo"
            },
            if english {
                "↑↓ select · Enter opens · Esc closes"
            } else {
                "↑↓ selecciona · Enter abre · Esc cierra"
            },
        ),
        crate::project::FinderMode::ProjectSearch => (
            if english {
                "Search in project"
            } else {
                "Buscar en el proyecto"
            },
            if english {
                "↑↓ select · Enter opens · Alt+R regex · Alt+C case · Alt+W word · Esc closes"
            } else {
                "↑↓ selecciona · Enter abre · Alt+R regex · Alt+C mayúsculas · Alt+W palabra · Esc cierra"
            },
        ),
    };
    let flags = match finder.mode {
        crate::project::FinderMode::ProjectSearch => format!(
            "{}{}{}",
            if finder.options.regex { " .*" } else { "" },
            if finder.options.case_sensitive {
                " Aa"
            } else {
                ""
            },
            if finder.options.whole_word {
                " \\b"
            } else {
                ""
            }
        ),
        crate::project::FinderMode::Files => String::new(),
    };
    div()
        .id("finder")
        .absolute()
        .top(px(TOPBAR_HEIGHT + 28.0))
        .left(px(48.0))
        .right(px(48.0))
        .flex()
        .justify_center()
        .child(
            div()
                .w(px(760.0))
                .p(px(12.0))
                .rounded(px(8.0))
                .bg(color(theme.chrome))
                .border_1()
                .border_color(color(theme.chrome_border))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .text_size(px(12.0))
                        .text_color(color(theme.muted))
                        .child(title)
                        .child(format!("{status}{flags}")),
                )
                .child(
                    div()
                        .text_size(px(14.0))
                        .text_color(color(theme.foreground))
                        .child(format!("› {}▏", finder.query)),
                )
                .children(rows.into_iter().enumerate().map(|(offset, row)| {
                    let selected = first + offset == finder.index;
                    div()
                        .px(px(8.0))
                        .py(px(4.0))
                        .rounded(px(4.0))
                        .flex()
                        .gap(px(12.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .bg(color(if selected {
                            theme.highlight
                        } else {
                            theme.chrome_active
                        }))
                        .text_size(px(13.0))
                        .child(finder_label(&row.label, &row.emphasis, theme))
                        .when(!row.detail.is_empty(), |item| {
                            item.child(
                                div()
                                    .text_color(color(theme.muted))
                                    .overflow_hidden()
                                    .child(row.detail),
                            )
                        })
                }))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(color(theme.muted))
                        .child(hint),
                ),
        )
        .into_any_element()
}

/// A finder row label with the fuzzy-matched characters in the accent
/// colour; runs of consecutive hits share one element.
fn finder_label(
    label: &str,
    emphasis: &[u32],
    theme: forge_gui::theme::ThemeColors,
) -> impl IntoElement {
    let mut pieces: Vec<(String, bool)> = Vec::new();
    for (index, c) in label.chars().enumerate() {
        let hit = emphasis
            .binary_search(&u32::try_from(index).unwrap_or(u32::MAX))
            .is_ok();
        match pieces.last_mut() {
            Some((text, last_hit)) if *last_hit == hit => text.push(c),
            _ => pieces.push((c.to_string(), hit)),
        }
    }
    div()
        .flex()
        .text_color(color(theme.foreground))
        .children(pieces.into_iter().map(|(text, hit)| {
            div()
                .when(hit, |piece| piece.text_color(color(theme.accent)))
                .child(text)
        }))
}

/// Find/replace bar of the active editor, top-right like the terminal's.
fn find_bar(view: &ForgeWindow) -> impl IntoElement {
    let theme = view.theme;
    let english = view.config.ui.language == Language::English;
    let find = &view.find;
    let status = find.error.clone().unwrap_or_else(|| find.label());
    let field = |label: &str, value: &str, focused: bool| {
        div()
            .flex()
            .gap(px(6.0))
            .text_size(px(13.0))
            .child(div().text_color(color(theme.muted)).child(label.to_owned()))
            .child(
                div()
                    .text_color(color(theme.foreground))
                    .child(format!("{value}{}", if focused { "▏" } else { "" })),
            )
    };
    div()
        .id("find-bar")
        .absolute()
        .top(px(TOPBAR_HEIGHT + 8.0))
        .right(px(24.0))
        .w(px(460.0))
        .px(px(10.0))
        .py(px(8.0))
        .rounded(px(8.0))
        .bg(color(theme.chrome))
        .border_1()
        .border_color(color(theme.chrome_border))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .flex()
                .justify_between()
                .child(field("⌕", &find.query, !find.replacing))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(color(if find.error.is_some() {
                            theme.danger
                        } else {
                            theme.accent
                        }))
                        .child(format!(
                            "{status}{}{}{}",
                            if find.options.regex { " .*" } else { "" },
                            if find.options.case_sensitive { " Aa" } else { "" },
                            if find.options.whole_word { " \\b" } else { "" }
                        )),
                ),
        )
        .when(find.replacing || !find.replacement.is_empty(), |bar| {
            bar.child(field("⇄", &find.replacement, find.replacing))
        })
        .child(
            div()
                .text_size(px(11.0))
                .text_color(color(theme.muted))
                .child(if english {
                    "Enter next · Shift+Enter previous · Tab field · Ctrl+Enter replace · Ctrl+Alt+Enter all · Alt+R/C/W"
                } else {
                    "Enter siguiente · Shift+Enter anterior · Tab campo · Ctrl+Enter reemplaza · Ctrl+Alt+Enter todos · Alt+R/C/W"
                }),
        )
}

/// Modal question centred over the panes; answered from the keyboard
/// (`Enter`/`y`, `Esc`/`n`, `a` for "always in this tab").
fn confirmation_dialog(
    view: &ForgeWindow,
    confirmation: &crate::window::Confirmation,
) -> impl IntoElement {
    let theme = view.theme;
    let english = view.config.ui.language == Language::English;
    let remember = matches!(confirmation.kind, ConfirmationKind::Clipboard { .. });
    let hint = match (english, remember) {
        (true, true) => "Enter/Y allow · A allow for this tab · Esc/N deny",
        (true, false) => "Enter/Y paste · Esc/N cancel",
        (false, true) => "Enter/Y permitir · A permitir en esta pestaña · Esc/N denegar",
        (false, false) => "Enter/Y pegar · Esc/N cancelar",
    };
    div()
        .id("confirmation")
        .absolute()
        .top(px(TOPBAR_HEIGHT + 60.0))
        .left(px(48.0))
        .right(px(48.0))
        .flex()
        .justify_center()
        .child(
            div()
                .w(px(520.0))
                .p(px(14.0))
                .rounded(px(8.0))
                .bg(color(theme.chrome))
                .border_1()
                .border_color(color(theme.chrome_active_border))
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .text_size(px(14.0))
                        .text_color(color(theme.foreground))
                        .child(confirmation.title.clone()),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(color(theme.muted))
                        .child(confirmation.body.clone()),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(color(theme.accent))
                        .child(hint),
                ),
        )
}

/// Find bar over the top-right corner of the pane area. Enter walks towards
/// older rows (the direction a search from the prompt wants), Shift+Enter
/// back; Alt+R / Alt+C toggle regex and case sensitivity.
fn search_bar(view: &ForgeWindow, cx: &mut Context<ForgeWindow>) -> impl IntoElement {
    let theme = view.theme;
    let english = view.config.ui.language == Language::English;
    let search = &view.search;
    let position = search.position_label();
    let status = match &search.error {
        Some(error) => error.clone(),
        None if search.query.is_empty() => {
            if english {
                "type to search".into()
            } else {
                "escribe para buscar".into()
            }
        }
        None if search.matches.is_empty() => {
            if english {
                "no matches".into()
            } else {
                "sin coincidencias".into()
            }
        }
        None => position.clone(),
    };
    div()
        .id("search-bar")
        .absolute()
        .top(px(TOPBAR_HEIGHT + 8.0))
        .right(px(24.0))
        .w(px(420.0))
        .px(px(10.0))
        .py(px(8.0))
        .rounded(px(8.0))
        .bg(color(theme.chrome))
        .border_1()
        .border_color(color(theme.chrome_border))
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(14.0))
                        .text_color(color(theme.foreground))
                        .child(format!("⌕ {}▏", search.query)),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(color(if search.error.is_some() {
                            theme.danger
                        } else {
                            theme.accent
                        }))
                        .whitespace_nowrap()
                        .child(status),
                )
                .children(search_toggles(view, cx)),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(color(theme.muted))
                .child(if english {
                    "Enter/↑ older · Shift+Enter/↓ newer · Alt+R regex · Alt+C case · Esc closes"
                } else {
                    "Enter/↑ anterior · Shift+Enter/↓ siguiente · Alt+R regex · Alt+C mayúsculas · Esc cierra"
                }),
        )
}

/// Regex and case toggles plus the close button of the search bar.
fn search_toggles(view: &ForgeWindow, cx: &mut Context<ForgeWindow>) -> Vec<AnyElement> {
    let theme = view.theme;
    let options = view.search.options;
    let toggle = |id: &'static str, label: &'static str, on: bool| {
        div()
            .id(id)
            .px(px(6.0))
            .py(px(2.0))
            .rounded(px(4.0))
            .text_size(px(11.0))
            .cursor_pointer()
            .bg(color(if on {
                theme.highlight
            } else {
                theme.chrome_active
            }))
            .text_color(color(if on { theme.foreground } else { theme.muted }))
            .child(label)
    };
    vec![
        toggle("search-regex", ".*", options.regex)
            .on_click(cx.listener(|view, _, _, cx| {
                view.search.options.regex = !view.search.options.regex;
                view.resubmit_search(cx);
            }))
            .into_any_element(),
        toggle("search-case", "Aa", options.case_sensitive)
            .on_click(cx.listener(|view, _, _, cx| {
                view.search.options.case_sensitive = !view.search.options.case_sensitive;
                view.resubmit_search(cx);
            }))
            .into_any_element(),
        div()
            .id("search-close")
            .cursor_pointer()
            .text_color(color(theme.muted))
            .child("×")
            .on_click(cx.listener(|view, _, _, cx| view.close_search_click(cx)))
            .into_any_element(),
    ]
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
