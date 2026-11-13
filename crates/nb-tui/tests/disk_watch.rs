//! Edits to the notebook file on disk are picked up by the periodic tick,
//! which reads a background watch instead of stat-ing on the UI thread.

use std::fs;
use std::time::{Duration, Instant, SystemTime};

use nb_core::{Document, FormatBacking};
use nb_kernel::FakeKernel;
use nb_sdk::ExtensionRegistry;
use nb_tui::{BuiltinsPlugin, Coordinator};

fn first_source(c: &Coordinator) -> String {
    let id = c.document.order()[0].clone();
    c.document.cell(&id).unwrap().buffer.as_str().to_string()
}

/// Tick until the first cell reads `expected`, for up to 3 s.
fn tick_until(c: &mut Coordinator, expected: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        c.on_tick(Instant::now());
        if first_source(c) == expected {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn external_edits_reload_from_the_tick() {
    let path = std::env::temp_dir().join(format!("nb-disk-watch-{}.py", std::process::id()));
    fs::write(&path, "# %%\nprint('A')\n").unwrap();
    let mut registry = ExtensionRegistry::default();
    registry.install(BuiltinsPlugin);
    let (kernel, _handle) = FakeKernel::pair();
    let mut c = Coordinator::headless(
        Document::new_empty(FormatBacking::PercentPy { header: None }),
        registry,
        Box::new(kernel),
    );
    c.path = Some(path.clone());
    assert!(tick_until(&mut c, "print('A')"), "first look at the file");

    fs::write(&path, "# %%\nprint('B')\n").unwrap();
    let later = SystemTime::now() + Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert!(tick_until(&mut c, "print('B')"), "change picked up");
    let _ = fs::remove_file(&path);
}
