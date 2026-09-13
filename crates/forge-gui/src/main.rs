use forge_gui::TerminalGrid;
use gpui::{App, Application, Bounds, Context, Render, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size};

struct ForgeWindow {
    grid: TerminalGrid,
}

impl Render for ForgeWindow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let (cols, rows) = self.grid.dimensions();
        div()
            .size_full()
            .bg(rgb(0x111318))
            .text_color(rgb(0xd8dee9))
            .p_4()
            .font_family("monospace")
            .child(format!("Forge terminal prototype · {cols}×{rows}"))
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.0), px(600.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| ForgeWindow { grid: TerminalGrid::new(80, 24) }),
        )
        .expect("open Forge window");
        cx.activate(true);
    });
}
