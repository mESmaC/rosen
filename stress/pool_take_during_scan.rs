// PR #202 review: a pool take must not fail while a scan walks the workspace.
use rosen_client::workspace::limits::{Budget, Settings};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn fill(dir: &Path, files: usize) {
    for i in 0..files {
        let sub = dir.join(format!("d{}", i % 50)).join(format!("e{}", i % 7));
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join(format!("f{i}")), vec![b'x'; 100 + i % 50]).unwrap();
    }
}

#[test]
fn pool_take_during_unlocked_scan() {
    let root = tempfile::tempdir().unwrap();
    let limits = root.path().join("limits");
    let budget = Budget::open(
        &limits,
        Settings {
            materialisations: 4,
            disk_bytes: 1 << 40,
        },
    )
    .unwrap();
    let (a, b) = (root.path().join("ws-a"), root.path().join("ws-b"));
    drop(budget.acquire(&a, &root.path().join("stage-a"), 1).unwrap());
    fill(&a, 6000);
    let stop = Arc::new(AtomicBool::new(false));
    let driver = {
        let (stop, limits, root) = (stop.clone(), limits.clone(), root.path().to_path_buf());
        std::thread::spawn(move || {
            let budget = Budget::open(&limits, Settings::default()).unwrap();
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                n += 1;
                let probe = root.join(format!("probe-{n}"));
                drop(
                    budget
                        .acquire(&probe, &root.join(format!("pstage-{n}")), 0)
                        .unwrap(),
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let (mut failed, mut here_a, start) = (Vec::new(), true, Instant::now());
    while start.elapsed() < Duration::from_secs(20) {
        let (from, to) = if here_a { (&a, &b) } else { (&b, &a) };
        match budget.rename_workspace(from, to) {
            Ok(()) => here_a = !here_a,
            Err(error) => failed.push(error.kind()),
        }
        std::thread::sleep(Duration::from_millis(7));
    }
    stop.store(true, Ordering::Relaxed);
    driver.join().unwrap();
    assert!(failed.is_empty(), "pool takes failed: {failed:?}");
}
