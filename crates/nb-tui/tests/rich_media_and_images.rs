//! End-to-end integration tests for Kitty Unicode graphics across Markdown
//! cells and rich MIME outputs (`image/png`, `image/svg+xml`, `text/vnd.mermaid`,
//! `text/latex`, `text/markdown`), including async worker completion,
//! `UpdateDisplayData` remounting/double-buffering, and scroll-settle deferral.

use std::borrow::Cow;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use crossterm::event::{MouseEvent, MouseEventKind, KeyModifiers};
use nb_core::{Cell, CellKind, Document, FormatBacking, MimeBundle, TargetId};
use nb_kernel::{FakeKernel, KernelEvent, WireOutput};
use nb_sdk::{ExtensionRegistry, SettingId, SettingValue};
use nb_tui::platform::{MemoryClipboard, NoopTerminalEffects};
use nb_tui::{BuiltinsPlugin, Coordinator, SettingsStore};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Color};
use serde_json::{Map, json};

fn make_rgba_png(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        pixels.extend_from_slice(&color);
    }
    encoder
        .write_header()
        .expect("png header")
        .write_image_data(&pixels)
        .expect("png pixels");
    out
}

fn coordinator_with_kitty(doc: Document, nb_path: Option<PathBuf>) -> Coordinator {
    let mut registry = ExtensionRegistry::default();
    registry.install(BuiltinsPlugin);
    let mut settings = SettingsStore::in_memory(&registry);
    settings.set_value(
        &SettingId(Cow::Borrowed("display.images")),
        SettingValue::Text("kitty".into()),
        &registry,
    );
    let (kernel, _handle) = FakeKernel::pair();
    Coordinator::new(
        nb_path,
        doc,
        registry,
        settings,
        Box::new(kernel),
        Box::new(MemoryClipboard::default()),
        Box::new(NoopTerminalEffects),
    )
}

fn draw_to_buffer(c: &mut Coordinator, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    let _ = terminal
        .draw(|frame| c.draw(frame))
        .expect("coordinator draw");
    terminal.backend().buffer().clone()
}

fn buffer_placeholder_ids(buf: &Buffer) -> Vec<u32> {
    let mut ids = Vec::new();
    for cell in &buf.content {
        if cell.symbol().starts_with(nb_tui::images::PLACEHOLDER)
            && let Color::Rgb(r, g, b) = cell.fg
        {
            let id = (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
            if id != 0 && !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

fn wait_until_placeholders(
    c: &mut Coordinator,
    width: u16,
    height: u16,
    min_count: usize,
    timeout: Duration,
) -> Buffer {
    let deadline = Instant::now() + timeout;
    loop {
        let _ = c.on_tick(Instant::now());
        let buf = draw_to_buffer(c, width, height);
        if buffer_placeholder_ids(&buf).len() >= min_count || Instant::now() >= deadline {
            return buf;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

#[test]
fn matplotlib_png_output_renders_kitty_placeholders_and_toggles_cleanly() {
    let mut code = Cell::new_code();
    code.buffer
        .replace_all("import matplotlib.pyplot as plt\nplt.plot([1, 2, 3])");
    let cell_id = code.id.clone();
    let doc = Document::from_cells(vec![code], FormatBacking::PercentPy { header: None });
    let mut c = coordinator_with_kitty(doc, None);

    let plot_png = make_rgba_png(640, 480, [40, 120, 220, 255]);
    let plot_b64 = base64::engine::general_purpose::STANDARD.encode(&plot_png);
    let mut bundle = MimeBundle::new();
    let _ = bundle.insert("image/png".into(), json!(plot_b64));
    let _ = bundle.insert(
        "text/plain".into(),
        json!("<Figure size 640x480 with 1 Axes>"),
    );

    c.apply_kernel_event(KernelEvent::Output {
        exec_id: None,
        target: TargetId::Cell(cell_id.clone()),
        late: false,
        payload: WireOutput::Rich {
            bundle,
            metadata: Map::new(),
            display_id: Some("fig-1".into()),
            is_execute_result: false,
            execution_count: None,
        },
    });

    let _ = draw_to_buffer(&mut c, 100, 32);
    let buf = wait_until_placeholders(&mut c, 100, 32, 1, Duration::from_secs(5));
    let ids = buffer_placeholder_ids(&buf);
    assert!(
        !ids.is_empty(),
        "expected Matplotlib image/png output to render Kitty Unicode placeholders"
    );
    for &id in &ids {
        let packets = nb_tui::images::packets_for(id).expect("packets registered for plot id");
        assert!(!packets.is_empty());
        assert!(packets.iter().all(|pkt| pkt.len() < 4096));
    }

    // Live UpdateDisplayData with a new figure keeps the plot height without collapsing to 1 row.
    let updated_png = make_rgba_png(640, 480, [220, 80, 60, 255]);
    let updated_b64 = base64::engine::general_purpose::STANDARD.encode(&updated_png);
    let mut updated_bundle = MimeBundle::new();
    let _ = updated_bundle.insert("image/png".into(), json!(updated_b64));
    let _ = updated_bundle.insert(
        "text/plain".into(),
        json!("<Figure size 640x480 with 1 Axes>"),
    );
    let height_before = c.layout_cache.heights().total_height();
    c.apply_kernel_event(KernelEvent::UpdateDisplay {
        display_id: "fig-1".into(),
        bundle: updated_bundle,
        metadata: Map::new(),
    });
    let _ = draw_to_buffer(&mut c, 100, 32);
    let height_during_decode = c.layout_cache.heights().total_height();
    assert_eq!(
        height_during_decode, height_before,
        "live display_id plot update must double-buffer without collapsing height"
    );

    // Toggling display.images = "off" restores the text/plain representation immediately.
    c.settings.set_value(
        &SettingId(Cow::Borrowed("display.images")),
        SettingValue::Text("off".into()),
        &c.registry,
    );
    c.sync_terminal_images();
    let _ = c.on_tick(Instant::now());
    let off_buf = draw_to_buffer(&mut c, 100, 32);
    assert!(buffer_placeholder_ids(&off_buf).is_empty());
    let screen_text: String = off_buf.content.iter().map(|cell| cell.symbol()).collect();
    assert!(screen_text.contains("<Figure size 640x480 with 1 Axes>"));
}

#[test]
fn update_display_upgrades_plain_text_placeholder_into_svg_plot() {
    let code = Cell::new_code();
    let cell_id = code.id.clone();
    let doc = Document::from_cells(vec![code], FormatBacking::PercentPy { header: None });
    let mut c = coordinator_with_kitty(doc, None);

    let mut initial = MimeBundle::new();
    let _ = initial.insert("text/plain".into(), json!("rendering plot..."));
    c.apply_kernel_event(KernelEvent::Output {
        exec_id: None,
        target: TargetId::Cell(cell_id),
        late: false,
        payload: WireOutput::Rich {
            bundle: initial,
            metadata: Map::new(),
            display_id: Some("upgrade-me".into()),
            is_execute_result: false,
            execution_count: None,
        },
    });
    let _ = draw_to_buffer(&mut c, 90, 28);

    let mut upgraded = MimeBundle::new();
    let _ = upgraded.insert(
        "image/svg+xml".into(),
        json!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="80"><circle cx="80" cy="40" r="30" fill="royalblue"/></svg>"#),
    );
    let _ = upgraded.insert("text/plain".into(), json!("<SVG figure>"));
    c.apply_kernel_event(KernelEvent::UpdateDisplay {
        display_id: "upgrade-me".into(),
        bundle: upgraded,
        metadata: Map::new(),
    });

    let _ = draw_to_buffer(&mut c, 90, 28);
    let buf = wait_until_placeholders(&mut c, 90, 28, 1, Duration::from_secs(5));
    assert!(
        !buffer_placeholder_ids(&buf).is_empty(),
        "UpdateDisplayData from text/plain to image/svg+xml must remount RichImageRenderer"
    );
    let _ = nb_tui::images::configure(nb_tui::images::ImageMode::Off);
}

#[test]
fn markdown_cell_renders_mermaid_and_relative_image_and_defers_during_scroll() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("nb-rich-md-{}-{stamp}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let img_path = dir.join("local_plot.png");
    fs::write(&img_path, make_rgba_png(120, 60, [80, 200, 120, 255])).unwrap();
    let nb_path = dir.join("demo.ipynb");

    let md = Cell::new(
        CellKind::Markdown,
        "# Architecture\n\n```mermaid\nflowchart LR\n    A[Input] --> B[Plot]\n```\n\n![local](local_plot.png)",
    );
    let filler: Vec<Cell> = std::iter::once(md)
        .chain((0..15).map(|i| Cell::new(CellKind::Code, format!("x_{i} = {i}"))))
        .collect();
    let doc = Document::from_cells(filler, FormatBacking::PercentPy { header: None });
    let mut c = coordinator_with_kitty(doc, Some(nb_path));

    // Initial draw queues Mermaid + local image on background workers.
    let _ = draw_to_buffer(&mut c, 96, 36);

    // Simulate active scroll wheel input: poll_images must hold reflows while scrolling.
    c.on_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 10,
        row: 5,
        modifiers: KeyModifiers::NONE,
    });
    assert!(c.is_scrolling_actively());
    assert!(
        !c.poll_images(),
        "background image reflows must wait until active scrolling settles"
    );

    // Wait past the 160ms scroll settle window and verify placeholders appear.
    std::thread::sleep(Duration::from_millis(180));
    c.scroll_row = 0;
    let settled_buf = wait_until_placeholders(&mut c, 96, 36, 2, Duration::from_secs(5));
    let ids = buffer_placeholder_ids(&settled_buf);
    assert!(
        ids.len() >= 2,
        "expected both Mermaid diagram and notebook-relative local_plot.png to render: got {ids:?}"
    );

    let _ = nb_tui::images::configure(nb_tui::images::ImageMode::Off);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn markdown_and_rich_outputs_render_markdown_and_mermaid_mimes() {
    let code = Cell::new_code();
    let cell_id = code.id.clone();
    let doc = Document::from_cells(vec![code], FormatBacking::PercentPy { header: None });
    let mut c = coordinator_with_kitty(doc, None);

    let mut md_bundle = MimeBundle::new();
    let _ = md_bundle.insert(
        "text/markdown".into(),
        json!("## Summary\n\n```mermaid\ngraph TD\n    X --> Y\n```"),
    );
    let _ = md_bundle.insert(
        "text/plain".into(),
        json!("<IPython.core.display.Markdown object>"),
    );
    c.apply_kernel_event(KernelEvent::Output {
        exec_id: None,
        target: TargetId::Cell(cell_id.clone()),
        late: false,
        payload: WireOutput::Rich {
            bundle: md_bundle,
            metadata: Map::new(),
            display_id: None,
            is_execute_result: false,
            execution_count: None,
        },
    });

    let mut mmd_bundle = MimeBundle::new();
    let _ = mmd_bundle.insert("text/vnd.mermaid".into(), json!("flowchart LR\n    P --> Q"));
    c.apply_kernel_event(KernelEvent::Output {
        exec_id: None,
        target: TargetId::Cell(cell_id),
        late: false,
        payload: WireOutput::Rich {
            bundle: mmd_bundle,
            metadata: Map::new(),
            display_id: None,
            is_execute_result: false,
            execution_count: None,
        },
    });

    let _ = draw_to_buffer(&mut c, 100, 40);
    let buf = wait_until_placeholders(&mut c, 100, 40, 2, Duration::from_secs(5));
    let ids = buffer_placeholder_ids(&buf);
    assert!(
        ids.len() >= 2,
        "expected both text/markdown Mermaid block and text/vnd.mermaid output to render: {ids:?}"
    );
    let _ = nb_tui::images::configure(nb_tui::images::ImageMode::Off);
}

#[test]
fn latex_math_in_markdown_cells_and_text_latex_outputs_typesets_when_tex_is_available() {
    let has_tex = ["/Library/TeX/texbin", "/usr/texbin", "/usr/bin", "/opt/homebrew/bin"]
        .iter()
        .any(|dir| {
            PathBuf::from(dir).join("latex").is_file()
                && PathBuf::from(dir).join("dvipng").is_file()
        });
    if !has_tex {
        return;
    }

    let md = Cell::new(
        CellKind::Markdown,
        "Inline $\\alpha + \\beta$ and display:\n\n$$\n\\int_0^1 x^2\\,dx = \\frac{1}{3}\n$$",
    );
    let code = Cell::new_code();
    let code_id = code.id.clone();
    let doc = Document::from_cells(vec![md, code], FormatBacking::PercentPy { header: None });
    let mut c = coordinator_with_kitty(doc, None);

    let mut latex_bundle = MimeBundle::new();
    let _ = latex_bundle.insert("text/latex".into(), json!("$\\displaystyle E = mc^2$"));
    let _ = latex_bundle.insert(
        "text/plain".into(),
        json!("<IPython.core.display.Math object>"),
    );
    c.apply_kernel_event(KernelEvent::Output {
        exec_id: None,
        target: TargetId::Cell(code_id),
        late: false,
        payload: WireOutput::Rich {
            bundle: latex_bundle,
            metadata: Map::new(),
            display_id: None,
            is_execute_result: false,
            execution_count: None,
        },
    });

    let _ = draw_to_buffer(&mut c, 100, 36);
    let buf = wait_until_placeholders(&mut c, 100, 36, 3, Duration::from_secs(15));
    let ids = buffer_placeholder_ids(&buf);
    assert!(
        ids.len() >= 3,
        "expected inline math, display math, and text/latex output to render: got {ids:?}"
    );
    let _ = nb_tui::images::configure(nb_tui::images::ImageMode::Off);
}

#[tokio::test]
async fn live_zmq_kernel_enforces_inline_mplbackend_and_renders_rich_displays() {
    let live_env = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/live/.pixi/envs/default");
    if !live_env.join("bin/python").is_file() {
        return;
    }

    let mut registry = ExtensionRegistry::default();
    registry.install(BuiltinsPlugin);
    let mut settings = SettingsStore::in_memory(&registry);
    settings.set_value(
        &SettingId(Cow::Borrowed("display.images")),
        SettingValue::Text("kitty".into()),
        &registry,
    );

    // Resolve kernelspec inside the live pixi environment by creating a temp workdir
    // symlink `.venv -> tests/live/.pixi/envs/default` (zero global env mutation).
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let work = std::env::temp_dir().join(format!("nb-live-zmq-{}-{stamp}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&live_env, work.join(".venv")).unwrap();

    let code_src = concat!(
        "import os\n",
        "from IPython.display import SVG, display\n",
        "assert os.environ.get('MPLBACKEND') == 'module://matplotlib_inline.backend_inline', os.environ.get('MPLBACKEND')\n",
        "display(SVG('<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"120\" height=\"60\"><rect width=\"120\" height=\"60\" fill=\"#22aa88\"/></svg>'))\n",
        "print('mpl-inline-ok')\n",
    );
    let cell = Cell::new(CellKind::Code, code_src);
    let cell_id = cell.id.clone();
    let doc = Document::from_cells(vec![cell], FormatBacking::PercentPy { header: None });

    let kernel = nb_kernel::LocalZmqKernel::spawn(nb_kernel::KernelLaunchSpec {
        requested_name: "python3".into(),
        working_dir: work.clone(),
        existing_connection_file: None,
    });
    let mut c = Coordinator::new(
        None,
        doc,
        registry,
        settings,
        Box::new(kernel),
        Box::new(MemoryClipboard::default()),
        Box::new(NoopTerminalEffects),
    );

    // Wait for KernelEvent::Ready, then execute the cell.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !c.kernel_alive && Instant::now() < deadline {
        while let Some(ev) = c.kernel.try_recv() {
            c.apply_kernel_event(ev);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(c.kernel_alive, "live ipykernel failed to become ready");

    c.kernel
        .send(nb_kernel::KernelCmd::Execute {
            target: TargetId::Cell(cell_id.clone()),
            lane: nb_kernel::Lane::Main,
            code: code_src.into(),
            silent: false,
            store_history: true,
            stop_on_error: true,
        })
        .unwrap();

    let mut finished = false;
    while Instant::now() < deadline && !finished {
        while let Some(ev) = c.kernel.try_recv() {
            if matches!(
                &ev,
                KernelEvent::ExecutionFinished {
                    target: TargetId::Cell(done_id),
                    ok,
                    ..
                } if done_id == &cell_id && *ok
            ) {
                finished = true;
            }
            c.apply_kernel_event(ev);
        }
        let _ = c.on_tick(Instant::now());
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        finished,
        "cell execution failed or timed out: {:?}",
        c.document.cell(&cell_id).map(|cell| &cell.outputs)
    );

    let _ = draw_to_buffer(&mut c, 100, 32);
    let buf = wait_until_placeholders(&mut c, 100, 32, 1, Duration::from_secs(5));
    assert!(
        !buffer_placeholder_ids(&buf).is_empty(),
        "expected live ipykernel SVG display_data to render Kitty placeholders"
    );

    c.kernel.shutdown(Duration::from_secs(3)).await;
    let _ = nb_tui::images::configure(nb_tui::images::ImageMode::Off);
    let _ = fs::remove_dir_all(work);
}
