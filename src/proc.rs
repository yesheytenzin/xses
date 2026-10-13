//! Child-process helpers shared by the writers and the sync/clean paths.
use anyhow::{Context, Result};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Run a command to completion, killing it once `timeout` elapses so a
/// wedged CLI (or a locked database) cannot hang a sync.
pub fn output_with_timeout(mut command: Command, timeout: Duration) -> std::io::Result<Output> {
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("command exceeded {} seconds", timeout.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Parse `sqlite3 -json` output. An empty result set prints no bytes at all,
/// which is a valid empty list — not a parse failure.
pub fn json_rows(text: &str, what: &str) -> Result<Vec<serde_json::Value>> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(text.trim()).with_context(|| format!("parse {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_sqlite_json_output_is_an_empty_list() {
        // sqlite3 -json prints nothing for zero rows; that must not be a parse error.
        assert!(json_rows("", "test").unwrap().is_empty());
        assert!(json_rows("  \n", "test").unwrap().is_empty());
        assert_eq!(json_rows(r#"[{"id":"a"}]"#, "test").unwrap().len(), 1);
        assert!(json_rows("not json", "test").is_err());
    }

    #[test]
    fn timeouts_kill_slow_children() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let err = output_with_timeout(cmd, Duration::from_millis(200)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }
}
