//! Sampling a running server: its memory, whether it still has work in a
//! child process, whether it has gone quiet, and the median of a batch of
//! latencies. Shared by `test_bench.rs` and `test_soak.rs`, which measure the
//! same process in the same ways and must agree about what a settled server
//! looks like.

use std::time::{Duration, Instant};

use super::LspClient;

/// Resident set size in KiB, or `None` on a platform with neither reader.
pub fn rss_kib(pid: u32) -> Option<u64> {
    if cfg!(target_os = "linux") {
        let status = std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
        return status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok());
    }
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        return String::from_utf8_lossy(&out.stdout).trim().parse().ok();
    }
    None
}

/// Whether the server still has a child process — a `clojure -Spath`, a
/// `clj-kondo`, the shell wrapping either. Work in a child is work the server
/// is doing, however quiet its own log has gone.
pub fn has_children(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{}/task", pid)) else {
            return false;
        };
        return tasks.flatten().any(|task| {
            std::fs::read_to_string(task.path().join("children"))
                .is_ok_and(|c| !c.trim().is_empty())
        });
    }
    if cfg!(target_os = "macos") {
        return std::process::Command::new("pgrep")
            .args(["-P", &pid.to_string()])
            .output()
            .is_ok_and(|out| !out.stdout.is_empty());
    }
    false
}

pub fn median(samples: &mut [Duration]) -> Option<Duration> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    Some(samples[samples.len() / 2])
}

/// Whether nothing matching `methods` arrived for `window`.
pub fn quiet_for(client: &mut LspClient, methods: &[&str], window: Duration) -> bool {
    let deadline = Instant::now() + window;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return true;
        };
        let Ok(msg) = client.incoming.recv_timeout(remaining) else {
            return true;
        };
        let hit = methods.iter().any(|m| msg["method"] == *m);
        client.stash(msg);
        if hit {
            return false;
        }
    }
}
