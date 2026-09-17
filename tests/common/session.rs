//! One server and everything that has gone wrong with it, plus the rule for
//! when it counts as settled. Shared by the soak and the compare gate: both
//! drive one production server through many requests, and both need every
//! JSON-RPC error answer recorded rather than merely returned.
//!
//! Both test binaries compile this module, and neither uses all of it, so
//! `dead_code` is off here rather than per item.
#![allow(dead_code)]

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::sampling::{
    has_children, quiet_for, QUIET, STAGE2_LINES, STAGE3_ANNOUNCE_GRACE, STAGE3_LINES,
};
use super::LspClient;

/// A client plus the liveness record for the server behind it. Every request
/// goes through here: a JSON-RPC error answer is a liveness failure — the panic
/// guard answers a panicked handler with exactly one — so it is recorded rather
/// than merely returned. A request that never answers panics inside the client
/// and takes the run down with it, which is the same verdict by a louder route.
pub struct Session {
    pub client: LspClient,
    pub errors: Vec<String>,
}

impl Session {
    pub fn new(client: LspClient) -> Self {
        Self {
            client,
            errors: Vec::new(),
        }
    }

    /// A server on `root` under production settings — stage 3 runs and
    /// clj-kondo is spawned when installed, because that is what the machine
    /// under a real editor does.
    pub fn production(root: &Path) -> Self {
        Self::new(LspClient::start_production(root).with_request_timeout(REQUEST_TIMEOUT))
    }

    pub fn client(&mut self) -> &mut LspClient {
        &mut self.client
    }

    pub fn pid(&self) -> u32 {
        self.client.child.id()
    }

    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let msg = self.client.request_full(method, params.clone());
        if let Some(error) = msg.get("error") {
            self.errors
                .push(format!("{method} answered {error}: {params}"));
        }
        msg["result"].clone()
    }

    /// Whether the server process is still running. A handler that panics is
    /// answered by the guard, so a dead process here means something worse.
    pub fn alive(&mut self) -> bool {
        matches!(self.client.child.try_wait(), Ok(None))
    }
}

/// Generous, like the bench's: the point is to find divergence, not to fail on
/// a slow machine.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Settles a server the way the bench does: stage 2 (or stage 3, when it is
/// coming) has reported, no child process is still working, and nothing has
/// been logged for [`QUIET`]. Waiting for `Indexed` alone would compare a
/// settled server against one still indexing its libraries, and invent
/// divergences that are nothing but a race.
pub fn settle(session: &mut Session, deadline: Instant) -> String {
    let terminal: Vec<&str> = STAGE2_LINES
        .iter()
        .chain(STAGE3_LINES.iter())
        .copied()
        .collect();
    let remaining = |deadline: Instant| deadline.saturating_duration_since(Instant::now());
    let stage = session
        .client
        .log_line_within(&terminal, remaining(deadline));
    let mut note = match &stage {
        Some(line) => line.trim_start_matches("clj-pulse: ").to_string(),
        None => "no library stage reported".to_string(),
    };
    let stage3 = stage
        .as_deref()
        .is_some_and(|l| STAGE3_LINES.iter().any(|s| l.contains(s)));
    if !stage3
        && session
            .client
            .log_line_within(&["resolving classpath via"], STAGE3_ANNOUNCE_GRACE)
            .is_some()
    {
        match session
            .client
            .log_line_within(&STAGE3_LINES, remaining(deadline))
        {
            Some(line) => note = line.trim_start_matches("clj-pulse: ").to_string(),
            None => note.push_str("; stage 3 announced but never reported"),
        }
    }
    let pid = session.pid();
    loop {
        session.client.clear_notifications();
        let quiet = quiet_for(&mut session.client, &["window/logMessage"], QUIET);
        if quiet && !has_children(pid) {
            return note;
        }
        if Instant::now() >= deadline {
            return format!("{note}; never settled");
        }
    }
}
