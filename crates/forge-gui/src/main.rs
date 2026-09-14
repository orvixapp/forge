mod bench;
mod chrome;
mod editor;
mod grid_element;
mod ipc;
mod project;
mod search;
mod window;

use crate::{
    bench::{
        GuiMetrics, emit_metrics, process_pss_kib, spawn_grid_benchmark, spawn_idle_benchmark,
        spawn_panes_benchmark, spawn_typing_benchmark, synthetic_rust_source,
    },
    grid_element::CellMetrics,
    window::{ForgeWindow, WindowFactory, initial_cwd},
};
use forge_gui::{
    config::{Config, ConfigSources, resolve_font_family},
    shell::WindowSession,
};
use gpui::{App, Application, AssetSource, SharedString, px, size};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicU64},
    time::Instant,
};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        match path {
            "forge-logo.svg" => Ok(Some(std::borrow::Cow::Borrowed(include_bytes!(
                "../assets/forge-logo.svg"
            )))),
            _ => Ok(None),
        }
    }

    fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(vec!["forge-logo.svg".into()])
    }
}

/// Everything decided before the GPUI application starts.
struct Startup {
    config: Config,
    sources: ConfigSources,
    config_path: Option<PathBuf>,
    cwd: PathBuf,
    window_size: gpui::Size<gpui::Pixels>,
    headless: bool,
}

fn prepare_startup(options: &RunOptions) -> Startup {
    let _span = tracing::info_span!("startup").entered();
    let benchmark = options.benchmark_grid_frames.is_some()
        || options.benchmark_panes.is_some()
        || options.benchmark_typing.is_some();
    let headless =
        benchmark || options.benchmark_idle_ms.is_some() || options.exit_after_first_frame;
    // Benchmarks stay comparable across machines by ignoring user config.
    let config_path = if benchmark {
        None
    } else {
        Config::default_path(options.config.as_deref())
    };
    let cwd = initial_cwd(None);
    let (config, sources) = {
        let _span = tracing::info_span!("config.load").entered();
        load_config(config_path.as_deref(), &cwd)
    };
    let session = if headless {
        None
    } else {
        config_path
            .as_deref()
            .map(|path| path.with_file_name("session.json"))
            .and_then(|path| WindowSession::load(&path).ok().flatten())
    };
    let window_size = match (&session, benchmark) {
        (_, true) => size(px(1300.0), px(700.0)),
        (Some(session), false) => size(
            px(session.width.clamp(480.0, 7680.0)),
            px(session.height.clamp(320.0, 4320.0)),
        ),
        (None, false) => size(px(960.0), px(600.0)),
    };
    let cwd = initial_cwd(session.as_ref().map(|session| session.cwd.as_path()));
    Startup {
        config,
        sources,
        config_path,
        cwd,
        window_size,
        headless,
    }
}

fn main() {
    let started = Instant::now();
    let options = run_options();
    if options.print_config_schema {
        println!("{}", Config::json_schema());
        return;
    }
    let _tracing = init_tracing();
    let startup = prepare_startup(&options);
    let render_count = Arc::new(AtomicU64::new(0));
    let panes = options
        .benchmark_panes
        .map(|count| (count, options.benchmark_frames));

    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            let _span = tracing::info_span!("app.run").entered();
            let Startup {
                mut config,
                sources,
                config_path,
                cwd,
                window_size,
                headless,
            } = startup;
            let family = {
                let _span = tracing::info_span!("font.resolve").entered();
                resolve_font_family(&config.font.family, &cx.text_system().all_font_names())
            };
            if !family.eq_ignore_ascii_case(&config.font.family) {
                eprintln!(
                    "forge: fuente {:?} no disponible como familia; usando {family:?}",
                    config.font.family
                );
            }
            config.font.family = family;
            let config = Arc::new(config);
            // The grid benchmark needs every one of its 200×60 cells on screen.
            let metrics = if options.benchmark_grid_frames.is_some() {
                CellMetrics::BENCHMARK
            } else {
                cell_metrics_for(&config, cx)
            };
            let factory = WindowFactory {
                config_path,
                config,
                sources,
                socket: options.socket.clone(),
                cwd,
                metrics,
                window_size,
                render_count: Arc::clone(&render_count),
                start_ipc: !headless,
                restore_session: !headless,
            };
            let window = {
                let _span = tracing::info_span!("window.open").entered();
                factory.open(cx).expect("open Forge window")
            };
            if !options.files.is_empty() {
                let files = options.files.clone();
                window
                    .update(cx, |view, _, cx| {
                        for file in &files {
                            let (path, line, column) = forge_gui::links::split_file_reference(file);
                            view.open_file(std::path::Path::new(path), line, column, cx);
                        }
                    })
                    .expect("open files from the command line");
            }
            if options.exit_after_first_frame {
                window
                    .update(cx, |_, window, _| {
                        window.on_next_frame(move |_, cx| {
                            emit_metrics(&GuiMetrics {
                                scenario: "startup_empty",
                                elapsed_ms: started.elapsed().as_secs_f64() * 1_000.0,
                                samples_ms: None,
                                frames: 1,
                                pss_kib: process_pss_kib(),
                                painted_cells: None,
                            });
                            cx.quit();
                        });
                    })
                    .expect("schedule startup measurement");
            }
            spawn_benchmarks(&options, panes, window, &render_count, cx);
            cx.activate(true);
        });
}

/// Starts whichever `--benchmark-*` scenario was requested.
fn spawn_benchmarks(
    options: &RunOptions,
    panes: Option<(usize, usize)>,
    window: gpui::WindowHandle<ForgeWindow>,
    render_count: &Arc<AtomicU64>,
    cx: &mut gpui::App,
) {
    if let Some(idle_ms) = options.benchmark_idle_ms {
        spawn_idle_benchmark(idle_ms, Arc::clone(render_count), cx);
    }
    if let Some(iterations) = options.benchmark_grid_frames {
        spawn_grid_benchmark(iterations, window, cx);
    }
    if let Some((count, frames)) = panes {
        spawn_panes_benchmark(count, frames, window, cx);
    }
    if let Some(keystrokes) = options.benchmark_typing {
        let path = std::env::temp_dir().join("forge-bench-typing.rs");
        std::fs::write(&path, synthetic_rust_source(10_000)).expect("write the synthetic source");
        window
            .update(cx, |view, _, cx| {
                view.open_file(&path, Some(5_000), Some(5), cx);
            })
            .expect("open the benchmark file");
        spawn_typing_benchmark(keystrokes, window, cx);
    }
}

/// `FORGE_LOG` filters `tracing` output on stderr; `FORGE_TRACE_FILE` writes
/// a Chrome trace (loadable in Perfetto) of the spans in this process.
fn init_tracing() -> Option<tracing_chrome::FlushGuard> {
    let trace_file = std::env::var_os("FORGE_TRACE_FILE");
    let default_level = if trace_file.is_some() { "info" } else { "warn" };
    let filter =
        EnvFilter::try_from_env("FORGE_LOG").unwrap_or_else(|_| EnvFilter::new(default_level));
    let registry = tracing_subscriber::registry().with(filter);
    if let Some(path) = trace_file {
        let (chrome, guard) = tracing_chrome::ChromeLayerBuilder::new()
            .file(path)
            .include_args(true)
            .build();
        registry
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(chrome)
            .init();
        Some(guard)
    } else {
        registry
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .init();
        None
    }
}

fn load_config(path: Option<&Path>, workspace: &Path) -> (Config, ConfigSources) {
    match Config::load_layers(path, Some(workspace)) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("forge: {error}; usando la configuración por defecto");
            (Config::default(), ConfigSources::default())
        }
    }
}

/// Cell geometry for the configured font: the advance of a monospace glyph
/// rounded to whole pixels, and the line height as a multiple of the size.
pub fn cell_metrics_for(config: &Config, cx: &App) -> CellMetrics {
    let font_size = px(config.font.size);
    let font_id = cx
        .text_system()
        .resolve_font(&gpui::font(config.font.family.clone()));
    let advance = cx
        .text_system()
        .advance(font_id, font_size, 'M')
        .map_or(font_size * 0.6, |size| size.width);
    CellMetrics {
        width: f32::from(advance).round().max(1.0),
        height: (config.font.size * config.font.line_height)
            .round()
            .max(1.0),
        font_size: config.font.size,
    }
}

#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn grid_dimensions(
    size: gpui::Size<gpui::Pixels>,
    metrics: CellMetrics,
    chrome_height: f32,
) -> (u16, u16) {
    let cols = ((size.width / px(metrics.width)).floor() as u16).clamp(1, 500);
    let rows =
        (((size.height - px(chrome_height)) / px(metrics.height)).floor() as u16).clamp(1, 300);
    (cols, rows)
}

struct RunOptions {
    socket: PathBuf,
    config: Option<PathBuf>,
    print_config_schema: bool,
    exit_after_first_frame: bool,
    benchmark_idle_ms: Option<u64>,
    benchmark_grid_frames: Option<usize>,
    benchmark_panes: Option<usize>,
    benchmark_frames: usize,
    /// Keystrokes to type into a synthetic 10k-line Rust file.
    benchmark_typing: Option<usize>,
    /// Files to open in editor tabs, as `path` or `path:line[:col]`.
    files: Vec<String>,
}

fn run_options() -> RunOptions {
    let mut args = std::env::args().skip(1);
    let mut options = RunOptions {
        socket: default_socket(),
        config: None,
        print_config_schema: false,
        exit_after_first_frame: false,
        benchmark_idle_ms: None,
        benchmark_grid_frames: None,
        benchmark_panes: None,
        benchmark_frames: 120,
        benchmark_typing: None,
        files: Vec::new(),
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => {
                if let Some(path) = args.next() {
                    options.socket = path.into();
                }
            }
            "--config" => options.config = args.next().map(PathBuf::from),
            "--print-config-schema" => options.print_config_schema = true,
            "--exit-after-first-frame" => options.exit_after_first_frame = true,
            "--benchmark-idle-ms" => {
                options.benchmark_idle_ms = args.next().and_then(|value| value.parse().ok());
            }
            "--benchmark-grid-frames" => {
                options.benchmark_grid_frames = args.next().and_then(|value| value.parse().ok());
            }
            "--benchmark-panes" => {
                options.benchmark_panes = args.next().and_then(|value| value.parse().ok());
            }
            "--benchmark-typing" => {
                options.benchmark_typing = args.next().and_then(|value| value.parse().ok());
            }
            "--benchmark-frames" => {
                if let Some(frames) = args.next().and_then(|value| value.parse().ok()) {
                    options.benchmark_frames = frames;
                }
            }
            other if !other.starts_with("--") => options.files.push(other.to_owned()),
            _ => {}
        }
    }
    options
}

fn default_socket() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"\\.\pipe\forge-prototype")
    } else {
        PathBuf::from("/tmp/forge-prototype.sock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `docs/config.schema.json` is what editors validate against; keep it
    /// in sync by regenerating with `forge-gui --print-config-schema`.
    #[test]
    fn committed_config_schema_matches_the_types() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/config.schema.json");
        let committed = std::fs::read_to_string(&path).expect("docs/config.schema.json exists");
        assert_eq!(
            committed.trim(),
            Config::json_schema().trim(),
            "regenerate docs/config.schema.json with `forge-gui --print-config-schema`"
        );
    }

    #[test]
    fn socket_argument_uses_documented_default_shape() {
        assert!(
            default_socket()
                .to_string_lossy()
                .contains("forge-prototype")
        );
    }

    #[test]
    fn derives_grid_dimensions_from_window_size_and_cell_metrics() {
        let metrics = CellMetrics {
            width: 9.0,
            height: 18.0,
            font_size: 14.0,
        };
        assert_eq!(
            grid_dimensions(size(px(900.0), px(416.0)), metrics, 56.0),
            (100, 20)
        );
        assert_eq!(
            grid_dimensions(size(px(1300.0), px(700.0)), CellMetrics::BENCHMARK, 0.0),
            (216, 70)
        );
        assert_eq!(
            grid_dimensions(size(px(1.0), px(1.0)), metrics, 56.0),
            (1, 1)
        );
    }
}
