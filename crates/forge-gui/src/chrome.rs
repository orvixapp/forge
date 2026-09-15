//! Rendering of the window chrome: top bar with tabs, window controls and
//! resize handles, the pane tree, the status lines, notifications, the
//! command palette and the process explorer.

use crate::{
    agent::{MessageRole, TimelineItem, ToolState},
    grid_element::{TerminalGridElement, color},
    window::{ConfirmationKind, ForgeWindow, NotificationLevel, TabContent},
};
use forge_gui::i18n::{tr, trf};
use forge_gui::shell::{PaneTree, Rect, ShellCommand, search_commands};
use gpui::{
    Animation, AnimationExt, AnyElement, Context, CursorStyle, ImageSource, MouseButton,
    ResizeEdge, Resource, SharedString, Window, div, img, prelude::*, px, rgb,
};
use std::time::Duration;

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
            root.child(picker_overlay(view, picker, cx))
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
            chrome_button("forge-settings", "⚙", theme.chrome_active, view).on_click(cx.listener(
                |view, _, window, cx| {
                    view.run_shell_command(ShellCommand::OpenSettings, window, cx);
                },
            )),
        )
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
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |view, event: &gpui::MouseDownEvent, _, cx| {
                view.activate_tab(index, cx);
                view.open_context_menu_for(crate::window::MenuTarget::Tab, event.position, cx);
                cx.stop_propagation();
            }),
        )
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
        .map(|chord| trf("{} · new tab", &[&chord]))
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
            agent_panel(agent, tab.id, theme, cx).into_any_element(),
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
            div().mt(px(4.0)).text_color(color(theme.muted)).child(trf(
                "theme {} · font {} {}px · config {}",
                &[
                    &view.theme_name,
                    &view.config.font.family,
                    &view.config.font.size,
                    &view
                        .factory
                        .sources
                        .user
                        .as_deref()
                        .map_or_else(|| "—".into(), |path| path.display().to_string()),
                ],
            )),
        )
        .children(view.tabs.iter().map(|tab| {
            let size = match &tab.content {
                TabContent::Terminal(terminal) => {
                    let (cols, rows) = terminal.terminal.grid.dimensions();
                    format!("{cols}×{rows}")
                }
                TabContent::Editor(editor) => trf("{} lines", &[&editor.buffer.len_lines()]),
                TabContent::Agent(agent) => trf("{} events", &[&agent.timeline.len()]),
            };
            div()
                .mt(px(4.0))
                .child(format!("{} · {size} · {}", tab.title(), tab.status()))
        }))
}

#[allow(clippy::too_many_lines)]
fn agent_panel(
    agent: &crate::agent::AgentTab,
    tab_id: u64,
    theme: forge_gui::theme::ThemeColors,
    cx: &mut Context<ForgeWindow>,
) -> impl IntoElement {
    let turn_active = agent.turn_active();
    let session = agent
        .session_id
        .as_deref()
        .map(|id| id.chars().take(12).collect::<String>());
    div()
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(
            div()
                .h(px(52.0))
                .flex_none()
                .px(px(18.0))
                .flex()
                .items_center()
                .justify_between()
                .border_b_1()
                .border_color(color(theme.chrome_border))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(9.0))
                        .child(agent_activity_dot(turn_active, tab_id, theme))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(color(theme.foreground))
                                .child(agent.agent_name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(color(theme.muted))
                                .child("ACP"),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .text_size(px(10.0))
                        .text_color(color(theme.muted))
                        .when(!agent.route.is_empty(), |row| {
                            row.child(agent.route.clone())
                        })
                        .when_some(session, |row, id| row.child(format!("#{id}"))),
                ),
        )
        .when(!agent.context.is_empty(), |panel| {
            panel.child(
                div()
                    .px(px(18.0))
                    .pt(px(9.0))
                    .flex()
                    .gap(px(6.0))
                    .overflow_hidden()
                    .children(agent.context.iter().map(|context| {
                        div()
                            .px(px(7.0))
                            .py(px(2.0))
                            .rounded(px(3.0))
                            .bg(with_alpha(color(theme.chrome), 0.65))
                            .text_color(color(theme.muted))
                            .text_size(px(10.0))
                            .child(format!("@{}", context.label))
                    })),
            )
        })
        .children(
            agent
                .pending_permissions
                .iter()
                .map(|pending| agent_permission_card(pending, tab_id, theme, cx)),
        )
        .children(
            agent
                .proposed_edits
                .iter()
                .enumerate()
                .map(|(edit_idx, proposed)| {
                    agent_proposed_edit_view(proposed, tab_id, edit_idx, theme, cx)
                }),
        )
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .id(("agent-timeline", tab_id))
                .px(px(18.0))
                .py(px(16.0))
                .overflow_y_scroll()
                .overflow_x_hidden()
                .track_scroll(&agent.scroll_handle)
                .flex()
                .flex_col()
                .gap(px(16.0))
                .children(agent.timeline.iter().enumerate().map(|(index, item)| {
                    let last = index + 1 == agent.timeline.len();
                    agent_timeline_item(
                        item,
                        index,
                        agent.item_selected(index),
                        turn_active && last,
                        tab_id,
                        theme,
                        cx,
                    )
                })),
        )
        .child(
            div()
                .flex_none()
                .mx(px(18.0))
                .mb(px(12.0))
                .border_1()
                .border_color(color(if turn_active {
                    theme.chrome_border
                } else {
                    theme.chrome_active_border
                }))
                .rounded(px(5.0))
                .bg(with_alpha(color(theme.chrome), 0.35))
                .child(
                    div()
                        .min_h(px(44.0))
                        .px(px(12.0))
                        .py(px(9.0))
                        .flex()
                        .gap(px(9.0))
                        .child(div().text_color(color(theme.accent)).child(if turn_active {
                            "·"
                        } else {
                            ">"
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .text_color(color(if agent.prompt.is_empty() {
                                    theme.muted
                                } else {
                                    theme.foreground
                                }))
                                .child(if agent.prompt.is_empty() {
                                    if turn_active {
                                        tr("The agent is working…").to_owned()
                                    } else {
                                        format!(
                                            "{} ▏",
                                            tr("Ask about the code or request a change…")
                                        )
                                    }
                                } else {
                                    format!("{} ▏", agent.prompt)
                                }),
                        ),
                )
                .child(
                    div()
                        .px(px(12.0))
                        .pb(px(7.0))
                        .flex()
                        .justify_between()
                        .text_size(px(9.0))
                        .text_color(color(theme.muted))
                        .child(agent.status.clone())
                        .child(if turn_active {
                            tr("Esc stop").to_owned()
                        } else {
                            tr("Enter send · Shift+Enter newline").to_owned()
                        }),
                ),
        )
}

fn agent_activity_dot(
    active: bool,
    tab_id: u64,
    theme: forge_gui::theme::ThemeColors,
) -> AnyElement {
    let dot = div().size(px(7.0)).rounded(px(99.0)).bg(color(if active {
        theme.accent
    } else {
        theme.muted
    }));
    if active {
        let accent = color(theme.accent);
        dot.with_animation(
            SharedString::from(format!("agent-pulse-{tab_id}")),
            Animation::new(Duration::from_millis(1100)).repeat(),
            move |dot, delta| {
                let opacity = 0.35 + 0.65 * (1.0 - (delta * 2.0 - 1.0).abs());
                dot.bg(with_alpha(accent, opacity))
            },
        )
        .into_any_element()
    } else {
        dot.into_any_element()
    }
}

#[allow(clippy::too_many_arguments)]
fn agent_perm_button(
    id: SharedString,
    label: &'static str,
    bg: forge_gui::config::HexColor,
    text: forge_gui::config::HexColor,
    tab_id: u64,
    req_id: serde_json::Value,
    decision: proto_acp::PermissionDecision,
    ttl: proto_acp::PermissionTtl,
    cx: &mut Context<ForgeWindow>,
) -> impl IntoElement {
    div()
        .id(id)
        .px(px(10.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .bg(color(bg))
        .text_color(color(text))
        .text_size(px(11.0))
        .cursor_pointer()
        .child(label)
        .on_click(cx.listener(move |window, _, _, cx| {
            window.agent_resolve_permission(tab_id, &req_id, decision, ttl, cx);
        }))
}

fn agent_permission_buttons(
    pending: &crate::agent::PendingPermissionRequest,
    tab_id: u64,
    theme: forge_gui::theme::ThemeColors,
    cx: &mut Context<ForgeWindow>,
) -> impl IntoElement {
    let req_id = pending.id.clone();

    div()
        .flex()
        .gap(px(8.0))
        .mt(px(4.0))
        .child(agent_perm_button(
            SharedString::from(format!("perm-once-{tab_id}-{}", pending.id)),
            tr("Allow once"),
            theme.accent,
            theme.chrome,
            tab_id,
            req_id.clone(),
            proto_acp::PermissionDecision::Allow,
            proto_acp::PermissionTtl::Once,
            cx,
        ))
        .child(agent_perm_button(
            SharedString::from(format!("perm-sess-{tab_id}-{}", pending.id)),
            tr("Allow for this session"),
            theme.chrome_active,
            theme.foreground,
            tab_id,
            req_id.clone(),
            proto_acp::PermissionDecision::Allow,
            proto_acp::PermissionTtl::Session,
            cx,
        ))
        .child(agent_perm_button(
            SharedString::from(format!("perm-always-{tab_id}-{}", pending.id)),
            tr("Allow always"),
            theme.chrome_active,
            theme.foreground,
            tab_id,
            req_id.clone(),
            proto_acp::PermissionDecision::Allow,
            proto_acp::PermissionTtl::Always,
            cx,
        ))
        .child(agent_perm_button(
            SharedString::from(format!("perm-deny-{tab_id}-{}", pending.id)),
            tr("Reject"),
            theme.danger,
            theme.foreground,
            tab_id,
            req_id,
            proto_acp::PermissionDecision::Deny,
            proto_acp::PermissionTtl::Once,
            cx,
        ))
}

fn agent_permission_card(
    pending: &crate::agent::PendingPermissionRequest,
    tab_id: u64,
    theme: forge_gui::theme::ThemeColors,
    cx: &mut Context<ForgeWindow>,
) -> AnyElement {
    div()
        .px(px(12.0))
        .py(px(8.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(color(theme.accent))
        .bg(color(theme.chrome))
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .justify_between()
                .items_center()
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(color(theme.accent))
                        .child(trf("Permission request · {}", &[&pending.title])),
                )
                .child(
                    div()
                        .px(px(6.0))
                        .py(px(2.0))
                        .rounded(px(3.0))
                        .bg(color(theme.chrome_active))
                        .text_size(px(10.0))
                        .child(format!("{:?}", pending.capability)),
                ),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(color(theme.foreground))
                .child(if !pending.detail.is_empty() {
                    pending.detail.clone()
                } else if pending.scope.is_empty() {
                    trf("The agent asks to run {}", &[&pending.tool_name])
                } else {
                    trf("Target: {}", &[&pending.scope])
                }),
        )
        .child(agent_permission_buttons(pending, tab_id, theme, cx))
        .into_any_element()
}

fn agent_hunk_action_buttons(
    tab_id: u64,
    edit_idx: usize,
    hunk_id: usize,
    theme: forge_gui::theme::ThemeColors,
    cx: &mut Context<ForgeWindow>,
) -> impl IntoElement {
    div()
        .flex()
        .gap(px(6.0))
        .mt(px(2.0))
        .child(
            div()
                .id(SharedString::from(format!(
                    "accept-hunk-{tab_id}-{edit_idx}-{hunk_id}"
                )))
                .px(px(6.0))
                .py(px(2.0))
                .rounded(px(3.0))
                .bg(color(theme.accent))
                .text_color(color(theme.chrome))
                .text_size(px(10.0))
                .cursor_pointer()
                .child(tr("Accept"))
                .on_click(cx.listener(move |window, _, _, cx| {
                    window.agent_accept_hunk(tab_id, edit_idx, hunk_id, cx);
                })),
        )
        .child(
            div()
                .id(SharedString::from(format!(
                    "reject-hunk-{tab_id}-{edit_idx}-{hunk_id}"
                )))
                .px(px(6.0))
                .py(px(2.0))
                .rounded(px(3.0))
                .bg(color(theme.chrome))
                .text_color(color(theme.foreground))
                .text_size(px(10.0))
                .cursor_pointer()
                .child(tr("Reject"))
                .on_click(cx.listener(move |window, _, _, cx| {
                    window.agent_reject_hunk(tab_id, edit_idx, hunk_id, cx);
                })),
        )
}

fn agent_proposed_hunk_card(
    hunk: &forge_buffer::ProposedHunk,
    tab_id: u64,
    edit_idx: usize,
    theme: forge_gui::theme::ThemeColors,
    cx: &mut Context<ForgeWindow>,
) -> AnyElement {
    let hunk_id = hunk.id;
    let status_badge = match &hunk.status {
        forge_buffer::HunkStatus::Pending => {
            div().text_color(color(theme.accent)).child("Pendiente")
        }
        forge_buffer::HunkStatus::Accepted => {
            div().text_color(color(theme.git_added)).child("Aceptado")
        }
        forge_buffer::HunkStatus::Rejected => {
            div().text_color(color(theme.muted)).child("Rechazado")
        }
        forge_buffer::HunkStatus::Conflict(msg) => div()
            .text_color(color(theme.danger))
            .child(trf("Conflict: {}", &[&msg])),
    };

    div()
        .p(px(8.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(color(theme.chrome_active_border))
        .bg(color(theme.chrome_active))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .flex()
                .justify_between()
                .items_center()
                .text_size(px(11.0))
                .child(trf(
                    "Hunk #{} · Lines {}-{}",
                    &[
                        &(hunk.id + 1),
                        &(hunk.buffer_lines.start + 1),
                        &hunk.buffer_lines.end,
                    ],
                ))
                .child(status_badge),
        )
        .when(!hunk.old_text.is_empty(), |h| {
            h.child(
                div()
                    .p(px(4.0))
                    .rounded(px(2.0))
                    .text_size(px(11.0))
                    .text_color(color(theme.git_deleted))
                    .child(format!("- {}", hunk.old_text.trim_end())),
            )
        })
        .when(!hunk.new_text.is_empty(), |h| {
            h.child(
                div()
                    .p(px(4.0))
                    .rounded(px(2.0))
                    .text_size(px(11.0))
                    .text_color(color(theme.git_added))
                    .child(format!("+ {}", hunk.new_text.trim_end())),
            )
        })
        .when(
            matches!(
                hunk.status,
                forge_buffer::HunkStatus::Pending | forge_buffer::HunkStatus::Conflict(_)
            ),
            |h| {
                h.child(agent_hunk_action_buttons(
                    tab_id, edit_idx, hunk_id, theme, cx,
                ))
            },
        )
        .into_any_element()
}

fn agent_proposed_edit_view(
    proposed: &forge_buffer::ProposedEdit,
    tab_id: u64,
    edit_idx: usize,
    theme: forge_gui::theme::ThemeColors,
    cx: &mut Context<ForgeWindow>,
) -> AnyElement {
    let pending_cnt = proposed.pending_count();
    let conflict_cnt = proposed.conflict_count();
    let total_hunks = proposed.hunks.len();

    div()
        .px(px(12.0))
        .py(px(8.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(color(theme.chrome_border))
        .bg(color(theme.chrome))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .justify_between()
                .items_center()
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .items_center()
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(color(theme.foreground))
                                .child(trf("Proposed edit · {}", &[&proposed.path.display()])),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(color(theme.muted))
                                .child(format!(
                                    "({total_hunks} hunks, {pending_cnt} pendientes, {conflict_cnt} conflictos)"
                                )),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(6.0))
                        .child(
                            div()
                                .id(SharedString::from(format!("accept-all-{tab_id}-{edit_idx}")))
                                .px(px(8.0))
                                .py(px(2.0))
                                .rounded(px(4.0))
                                .bg(color(theme.accent))
                                .text_color(color(theme.chrome))
                                .text_size(px(11.0))
                                .cursor_pointer()
                                .child(tr("Accept all"))
                                .on_click(cx.listener(move |window, _, _, cx| {
                                    window.agent_accept_all_hunks(tab_id, edit_idx, cx);
                                })),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("reject-all-{tab_id}-{edit_idx}")))
                                .px(px(8.0))
                                .py(px(2.0))
                                .rounded(px(4.0))
                                .bg(color(theme.chrome_active))
                                .text_color(color(theme.foreground))
                                .text_size(px(11.0))
                                .cursor_pointer()
                                .child(tr("Reject all"))
                                .on_click(cx.listener(move |window, _, _, cx| {
                                    window.agent_reject_all_hunks(tab_id, edit_idx, cx);
                                })),
                        ),
                ),
        )
        .when(conflict_cnt > 0, |v| {
            v.child(
                div()
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(4.0))
                    .bg(color(theme.danger))
                    .text_color(color(theme.foreground))
                    .text_size(px(11.0))
                    .child(tr("⚠ Conflict detected: concurrent changes in the buffer. Review them before accepting.")),
            )
        })
        .children(
            proposed
                .hunks
                .iter()
                .map(|hunk| agent_proposed_hunk_card(hunk, tab_id, edit_idx, theme, cx)),
        )
        .into_any_element()
}

#[allow(clippy::too_many_lines)]
fn agent_timeline_item(
    item: &TimelineItem,
    index: usize,
    selected: bool,
    streaming: bool,
    tab_id: u64,
    theme: forge_gui::theme::ThemeColors,
    cx: &mut Context<ForgeWindow>,
) -> AnyElement {
    let (label, tint, body) = match item {
        TimelineItem::Message { role, text } => {
            let label = match role {
                MessageRole::User => tr("You"),
                MessageRole::Agent => tr("Agent"),
            };
            let body = match role {
                MessageRole::User => div()
                    .text_size(px(13.0))
                    .child(text.clone())
                    .into_any_element(),
                MessageRole::Agent => agent_markdown(text, streaming, theme),
            };
            (label.to_owned(), theme.foreground, body)
        }
        TimelineItem::Thought { text, active, .. } => {
            let label = if *active {
                tr("● Thinking…")
            } else {
                tr("Reasoning")
            };
            (
                label.to_owned(),
                theme.muted,
                div()
                    .text_size(px(12.0))
                    .text_color(color(theme.muted))
                    .child(text.clone())
                    .into_any_element(),
            )
        }
        TimelineItem::ToolCall {
            title,
            state,
            detail,
            ..
        } => {
            let state_label = match state {
                ToolState::Pending => tr("pending"),
                ToolState::Running => tr("running"),
                ToolState::Succeeded => tr("completed"),
                ToolState::Failed => tr("failed"),
                ToolState::WaitingPermission => tr("waiting for permission"),
            };
            (
                format!("▸ {} · {state_label}", tr("Tool")),
                theme.accent,
                agent_tool_body(title, detail, *state, theme),
            )
        }
        TimelineItem::Plan { title, entries } => (
            "Plan".to_owned(),
            theme.accent,
            agent_markdown(&format!("## {title}\n{}", entries.join("\n")), false, theme),
        ),
    };
    let thought = matches!(item, TimelineItem::Thought { .. });
    div()
        .id(("agent-message", index))
        .w_full()
        .px(px(8.0))
        .py(px(5.0))
        .rounded(px(3.0))
        .cursor(CursorStyle::IBeam)
        .when(selected, |row| {
            row.bg(with_alpha(color(theme.selection), theme.selection_opacity))
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |view, _, _, cx| {
                if let Some(agent) = view.active_tab_mut().agent_mut() {
                    agent.start_selection(index);
                    cx.notify();
                }
            }),
        )
        .on_mouse_move(
            cx.listener(move |view, event: &gpui::MouseMoveEvent, _, cx| {
                if event.pressed_button == Some(MouseButton::Left)
                    && let Some(agent) = view.active_tab_mut().agent_mut()
                {
                    agent.extend_selection(index);
                    cx.notify();
                }
            }),
        )
        .child(
            div()
                .child(
                    div()
                        .mb(px(4.0))
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .text_size(px(10.0))
                        .text_color(color(tint))
                        .child(label),
                )
                .child(body),
        )
        .when(streaming && thought, |row| {
            row.child(agent_activity_dot(true, tab_id.saturating_add(1), theme))
        })
        .into_any_element()
}

fn agent_tool_body(
    title: &str,
    detail: &str,
    state: ToolState,
    theme: forge_gui::theme::ThemeColors,
) -> AnyElement {
    let detail = tool_output_preview(detail, state);
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(color(theme.foreground))
                .child(title.to_owned()),
        )
        .when(!detail.is_empty(), |body| {
            body.child(
                div()
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(3.0))
                    .bg(with_alpha(color(theme.chrome), 0.65))
                    .text_size(px(11.0))
                    .text_color(color(if state == ToolState::Failed {
                        theme.danger
                    } else {
                        theme.muted
                    }))
                    .child(detail),
            )
        })
        .into_any_element()
}

fn tool_output_preview(detail: &str, state: ToolState) -> String {
    const LINES: usize = 8;
    let lines = detail.lines().collect::<Vec<_>>();
    if lines.len() <= LINES {
        return detail.to_owned();
    }
    let hidden = lines.len() - LINES;
    if state == ToolState::Running {
        trf(
            "… {} earlier lines\n{}",
            &[&hidden, &lines[hidden..].join("\n")],
        )
    } else {
        trf(
            "{}\n… {} more lines",
            &[&lines[..LINES].join("\n"), &hidden],
        )
    }
}

fn agent_markdown(
    source: &str,
    streaming: bool,
    theme: forge_gui::theme::ThemeColors,
) -> AnyElement {
    let source = if streaming {
        format!("{source} ▋")
    } else {
        source.to_owned()
    };
    let mut blocks = Vec::new();
    let mut code = Vec::new();
    let mut language = String::new();
    let mut in_code = false;
    for line in source.lines() {
        if let Some(info) = line.trim_start().strip_prefix("```") {
            if in_code {
                blocks.push(markdown_code_block(&language, &code.join("\n"), theme));
                code.clear();
                language.clear();
            } else {
                info.trim().clone_into(&mut language);
            }
            in_code = !in_code;
            continue;
        }
        if in_code {
            code.push(line);
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            blocks.push(div().h(px(5.0)).into_any_element());
        } else if let Some(heading) = trimmed.strip_prefix("### ") {
            blocks.push(markdown_heading(heading, 13.0, theme));
        } else if let Some(heading) = trimmed.strip_prefix("## ") {
            blocks.push(markdown_heading(heading, 14.0, theme));
        } else if let Some(heading) = trimmed.strip_prefix("# ") {
            blocks.push(markdown_heading(heading, 15.0, theme));
        } else if let Some(entry) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            blocks.push(
                div()
                    .flex()
                    .gap(px(8.0))
                    .text_size(px(13.0))
                    .child(div().text_color(color(theme.accent)).child("•"))
                    .child(div().flex_1().child(clean_inline_markdown(entry)))
                    .into_any_element(),
            );
        } else if let Some((number, entry)) = markdown_ordered_entry(trimmed) {
            blocks.push(
                div()
                    .flex()
                    .gap(px(8.0))
                    .text_size(px(13.0))
                    .child(
                        div()
                            .text_color(color(theme.accent))
                            .child(format!("{number}.")),
                    )
                    .child(div().flex_1().child(clean_inline_markdown(entry)))
                    .into_any_element(),
            );
        } else {
            blocks.push(
                div()
                    .text_size(px(13.0))
                    .text_color(color(theme.foreground))
                    .child(clean_inline_markdown(trimmed))
                    .into_any_element(),
            );
        }
    }
    if !code.is_empty() {
        blocks.push(markdown_code_block(&language, &code.join("\n"), theme));
    }
    div()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .children(blocks)
        .into_any_element()
}

fn markdown_ordered_entry(line: &str) -> Option<(&str, &str)> {
    let (number, entry) = line.split_once(". ")?;
    number
        .chars()
        .all(|character| character.is_ascii_digit())
        .then_some((number, entry))
}

fn markdown_heading(text: &str, size: f32, theme: forge_gui::theme::ThemeColors) -> AnyElement {
    div()
        .mt(px(3.0))
        .text_size(px(size))
        .text_color(color(theme.accent))
        .child(clean_inline_markdown(text))
        .into_any_element()
}

fn markdown_code_block(
    language: &str,
    code: &str,
    theme: forge_gui::theme::ThemeColors,
) -> AnyElement {
    div()
        .mt(px(4.0))
        .rounded(px(3.0))
        .bg(with_alpha(color(theme.chrome), 0.75))
        .when(!language.is_empty(), |block| {
            block.child(
                div()
                    .px(px(9.0))
                    .pt(px(6.0))
                    .text_size(px(9.0))
                    .text_color(color(theme.muted))
                    .child(language.to_owned()),
            )
        })
        .child(
            div()
                .px(px(9.0))
                .py(px(7.0))
                .text_size(px(11.0))
                .child(code.to_owned()),
        )
        .into_any_element()
}

fn clean_inline_markdown(text: &str) -> String {
    text.replace("**", "").replace("__", "").replace('`', "")
}

fn with_alpha(mut color: gpui::Rgba, opacity: f32) -> gpui::Rgba {
    color.a *= opacity;
    color
}

/// Right-click menu at the pointer: command titles with their chords.
fn context_menu(
    view: &ForgeWindow,
    menu: &crate::window::ContextMenu,
    cx: &mut Context<ForgeWindow>,
) -> impl IntoElement {
    // Wide enough for the longest translated title plus its chord; the
    // menu is then moved back inside the window when it would spill out.
    const MENU_WIDTH: f32 = 420.0;
    const ITEM_HEIGHT: f32 = 24.0;
    let theme = view.theme;
    let height = ITEM_HEIGHT * u16::try_from(menu.items.len()).map_or(f32::MAX, f32::from) + 8.0;
    let window_size = view.factory.window_size;
    let left =
        f32::from(menu.position.x).min((f32::from(window_size.width) - MENU_WIDTH - 4.0).max(0.0));
    let top =
        f32::from(menu.position.y).min((f32::from(window_size.height) - height - 4.0).max(0.0));
    div()
        .id("context-menu")
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(MENU_WIDTH))
        .py(px(4.0))
        .rounded(px(6.0))
        .bg(color(theme.chrome))
        .border_1()
        .border_color(color(theme.chrome_active_border))
        .text_size(px(12.0))
        // Clicks inside the menu must not reach the window handler, which
        // would close the menu on mouse down before the item's click fires.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .children(menu.items.iter().enumerate().map(|(index, command)| {
            let selected = index == menu.index;
            div()
                .id(("context-menu-item", index))
                .h(px(ITEM_HEIGHT))
                .px(px(10.0))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(24.0))
                .whitespace_nowrap()
                .overflow_hidden()
                .cursor_pointer()
                .bg(color(if selected {
                    theme.highlight
                } else {
                    theme.chrome
                }))
                .hover(move |style| style.bg(color(theme.highlight)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, _, window, cx| {
                        view.context_menu_pick(index, window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .child(tr(command.title())),
                )
                .child(
                    div()
                        .flex_shrink_0()
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
                .child(match prompt.kind {
                    crate::window::PromptKind::RenameTab => {
                        tr("Enter applies · empty restores the automatic name · Esc cancels")
                    }
                    _ => tr("Enter continues · Esc cancels"),
                }),
        )
}

fn picker_overlay(
    view: &ForgeWindow,
    picker: &crate::window::Picker,
    cx: &mut Context<ForgeWindow>,
) -> impl IntoElement {
    let theme = view.theme;
    if picker.kind == crate::window::PickerKind::LspInfo {
        return overlay_box(view, "lsp-info")
            .max_h(px(480.0))
            .overflow_y_scroll()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(picker.title.clone())
            .child(agent_markdown(
                picker.items.first().map_or("", String::as_str),
                false,
                theme,
            ))
            .child(tr("Esc closes"))
            .into_any_element();
    }
    overlay_box(view, "picker")
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .text_size(px(14.0))
                .text_color(color(theme.foreground))
                .child(picker.title.clone()),
        )
        .children(
            picker
                .items
                .iter()
                .enumerate()
                .skip(picker.index.saturating_sub(4))
                .take(9)
                .map(|(index, item)| {
                    let selected = index == picker.index;
                    div()
                        .id(("picker-item", index))
                        .px(px(8.0))
                        .py(px(5.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .hover(move |style| style.bg(color(theme.highlight)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |view, _, _, cx| {
                                view.picker_pick(index, cx);
                                cx.stop_propagation();
                            }),
                        )
                        .bg(color(if selected {
                            theme.highlight
                        } else {
                            theme.chrome_active
                        }))
                        .text_size(px(13.0))
                        .child(item.clone())
                }),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(color(theme.muted))
                .child(tr("↑↓ select · Enter opens · Esc closes")),
        )
        .into_any_element()
}

/// File finder / project search: a query line, a status line and the
/// matching rows, with the selected one highlighted.
#[allow(clippy::too_many_lines)]
fn finder_overlay(view: &ForgeWindow) -> AnyElement {
    let theme = view.theme;
    let Some(finder) = &view.finder else {
        return div().into_any_element();
    };
    let rows = view.finder_rows();
    let first = view.finder_first_row();
    let status = view.finder_status();
    let (title, hint) = match finder.mode {
        crate::project::FinderMode::Files => {
            (tr("Go to file"), tr("↑↓ select · Enter opens · Esc closes"))
        }
        crate::project::FinderMode::ProjectSearch => (
            tr("Search in project"),
            tr("↑↓ select · Enter opens · Alt+R regex · Alt+C case · Alt+W word · Esc closes"),
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
                .child(tr(
                    "Enter next · Shift+Enter previous · Tab field · Ctrl+Enter replace · Ctrl+Alt+Enter all · Alt+R/C/W",
                )),
        )
}

/// Modal question centred over the panes; answered from the keyboard
/// (`Enter`/`y`, `Esc`/`n`, `a` for "always in this tab").
fn confirmation_dialog(
    view: &ForgeWindow,
    confirmation: &crate::window::Confirmation,
) -> impl IntoElement {
    let theme = view.theme;
    let remember = matches!(confirmation.kind, ConfirmationKind::Clipboard { .. });
    let hint = if remember {
        tr("Enter/Y allow · A allow for this tab · Esc/N deny")
    } else {
        tr("Enter/Y paste · Esc/N cancel")
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
    let search = &view.search;
    let position = search.position_label();
    let status = match &search.error {
        Some(error) => error.clone(),
        None if search.query.is_empty() => tr("type to search").into(),
        None if search.matches.is_empty() => tr("no matches").into(),
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
                .child(tr(
                    "Enter/↑ older · Shift+Enter/↓ newer · Alt+R regex · Alt+C case · Esc closes",
                )),
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
                .child(tr("↑↓ select · Enter runs · Esc closes")),
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
                        .child(tr(item.command.title()))
                        .child(
                            div()
                                .text_color(color(theme.muted))
                                .child(view.keymap.chord_for(item.command).unwrap_or_default()),
                        )
                }),
        )
        .when(view.palette.query.is_empty(), |root| {
            root.child(
                div()
                    .mt(px(8.0))
                    .text_size(px(11.0))
                    .text_color(rgb(0x7f_8a_a3))
                    .child(tr("Type to filter commands")),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_helpers_remove_source_markers_and_recognise_lists() {
        assert_eq!(
            clean_inline_markdown("**Forge** y `cargo test`"),
            "Forge y cargo test"
        );
        assert_eq!(
            markdown_ordered_entry("12. elemento"),
            Some(("12", "elemento"))
        );
        assert_eq!(markdown_ordered_entry("no es lista"), None);
    }

    #[test]
    fn completed_tool_output_is_kept_compact() {
        let output = (1..=12)
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let preview = tool_output_preview(&output, ToolState::Succeeded);
        assert!(preview.contains('8'));
        assert!(!preview.contains("\n9\n"));
        assert!(preview.contains('4'));
    }
}
