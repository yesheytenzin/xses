//! Child-process helpers shared by the writers and the sync/clean paths.
use anyhow::{Context, Result};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Run a command to completion, killing it once `timeout` elapses so a
/// wedged CLI (or a locked database) cannot hang a sync.
pub fn output_with_timeout(command: Command, timeout: Duration) -> std::io::Result<Output> {
    run_with_timeout(command, timeout, None)
}

pub fn output_with_input(
    command: Command,
    input: Vec<u8>,
    timeout: Duration,
) -> std::io::Result<Output> {
    run_with_timeout(command, timeout, Some(input))
}

fn run_with_timeout(
    mut command: Command,
    timeout: Duration,
    input: Option<Vec<u8>>,
) -> std::io::Result<Output> {
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::mpsc;
    // Read both pipes while the child runs: waiting for exit first deadlocks
    // when either pipe fills. A channel deadline also bounds inherited pipes.
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    let (input_send, input_receive) = mpsc::channel();
    let mut input_done = input.is_none();
    if let Some(input) = input {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("child stdin pipe missing"))?;
        std::thread::spawn(move || {
            use std::io::Write;
            let result = stdin.write_all(&input);
            drop(stdin);
            let _ = input_send.send(result);
        });
    }
    let (send, receive) = mpsc::channel();
    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("child stdout pipe missing"))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("child stderr pipe missing"))?;
    let pipes: Vec<(bool, Box<dyn Read + Send>)> = vec![
        (true, Box::new(stdout_pipe)),
        (false, Box::new(stderr_pipe)),
    ];
    for (stdout, mut pipe) in pipes {
        let send = send.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = pipe.read_to_end(&mut bytes).map(|_| bytes);
            let _ = send.send((stdout, result));
        });
    }
    drop(send);
    let result = (|| {
        let mut stdout = None;
        let mut stderr = None;
        loop {
            if let Ok(result) = input_receive.try_recv() {
                result?;
                input_done = true;
            }
            while let Ok((is_stdout, bytes)) = receive.try_recv() {
                if is_stdout {
                    stdout = Some(bytes?);
                } else {
                    stderr = Some(bytes?);
                }
            }
            if let Some(status) = child.try_wait()? {
                if input_done && stdout.is_some() && stderr.is_some() {
                    let missing =
                        || std::io::Error::other("child pipe closed before delivering output");
                    return Ok(Output {
                        status,
                        stdout: stdout.take().ok_or_else(missing)?,
                        stderr: stderr.take().ok_or_else(missing)?,
                    });
                }
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("command exceeded {} seconds", timeout.as_secs_f64()),
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })();
    if result.is_err() {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

/// Quote one SQLite string literal. The `sqlite3` CLI has no bind
/// parameters, so every interpolated literal funnels through here: the
/// single quote is doubled, and ids are additionally allowlisted by
/// [`sql_quote_id`].
pub fn sql_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Quote a session/store id for interpolation. Ids are machine-made
/// (`ses_*`, uuids, slugs); anything outside that shape is refused instead
/// of escaped — a hostile id must fail loudly, never ride into SQL.
pub fn sql_quote_id(id: &str) -> Result<String> {
    let ok = !id.is_empty()
        && id.len() <= 256
        && id.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'.' | b':' | b'/' | b'@')
        });
    anyhow::ensure!(ok, "refusing to interpolate suspicious sqlite id: {id:?}");
    Ok(sql_quote(id))
}

/// Quote several ids as an `IN (...)` list. Empty input is a caller bug —
/// fail rather than emit `IN ()`, which sqlite rejects with a bare syntax
/// error far from the cause.
pub fn sql_id_list<'a>(ids: impl Iterator<Item = &'a str>) -> Result<String> {
    let mut quoted = Vec::new();
    for id in ids {
        quoted.push(sql_quote_id(id)?);
    }
    anyhow::ensure!(!quoted.is_empty(), "empty id list for sqlite IN clause");
    Ok(quoted.join(","))
}

/// Parse `sqlite3 -json` output. An empty result set prints no bytes at all,
/// which is a valid empty list — not a parse failure.
pub fn json_rows(text: &str, what: &str) -> Result<Vec<serde_json::Value>> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(text.trim()).with_context(|| format!("parse {what}"))
}

/// Read-only sqlite query (`-readonly -json`) with parsed rows. Bounded by
/// `timeout` so a wedged store cannot hang a scan.
pub fn sqlite_json(
    db: &std::path::Path,
    sql: &str,
    timeout: Duration,
    what: &str,
) -> Result<Vec<serde_json::Value>> {
    let mut command = Command::new("sqlite3");
    command.args(["-readonly", "-json"]).arg(db).arg(sql);
    let out = output_with_timeout(command, timeout)
        .with_context(|| format!("launch sqlite3 for {what}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    json_rows(&String::from_utf8_lossy(&out.stdout), what)
}

/// Read-only sqlite query returning trimmed text (probes like
/// `SELECT 1 …`). Status-checked and bounded like [`sqlite_json`].
pub fn sqlite_text(
    db: &std::path::Path,
    sql: &str,
    timeout: Duration,
    what: &str,
) -> Result<String> {
    let mut command = Command::new("sqlite3");
    command.args(["-readonly"]).arg(db).arg(sql);
    let out = output_with_timeout(command, timeout)
        .with_context(|| format!("launch sqlite3 for {what}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Write sqlite script passed as an argument: aborts on first error
/// (`-bail`), bounded by `timeout`.
pub fn sqlite_exec(db: &std::path::Path, sql: &str, timeout: Duration, what: &str) -> Result<()> {
    let mut command = Command::new("sqlite3");
    command
        .args(["-bail", "-cmd", ".timeout 5000"])
        .arg(db)
        .arg(sql);
    let out = output_with_timeout(command, timeout)
        .with_context(|| format!("launch sqlite3 for {what}"))?;
    anyhow::ensure!(
        out.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// Write sqlite script passed on stdin (for large generated batches).
/// Aborts on first error, bounded by `timeout`.
pub fn sqlite_exec_input(
    db: &std::path::Path,
    script: Vec<u8>,
    timeout: Duration,
    what: &str,
) -> Result<()> {
    let mut command = Command::new("sqlite3");
    command.args(["-bail", "-cmd", ".timeout 5000"]).arg(db);
    let out = output_with_input(command, script, timeout)
        .with_context(|| format!("launch sqlite3 for {what}"))?;
    anyhow::ensure!(
        out.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_id_quoting_rejects_hostile_input() {
        assert_eq!(super::sql_quote_id("ses_abc123").unwrap(), "'ses_abc123'");
        assert_eq!(
            super::sql_quote_id("01a1349e-571e-727e-b6e4-221162373935").unwrap(),
            "'01a1349e-571e-727e-b6e4-221162373935'"
        );
        // Classic injection: spaces and quotes are outside the id shape.
        assert!(super::sql_quote_id("' OR '1'='1").is_err());
        assert!(super::sql_quote_id("ses_x'; DROP TABLE session;--").is_err());
        assert!(super::sql_quote_id("").is_err());
        assert!(super::sql_id_list(["a", "b"].into_iter()).is_ok());
        assert!(super::sql_id_list(["a", "'"].into_iter()).is_err());
        assert!(super::sql_id_list([].into_iter()).is_err());
        // Free text still quotes by doubling.
        assert_eq!(super::sql_quote("o'Brien"), "'o''Brien'");
    }

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
        let cmd = fixture("sleep");
        let err = output_with_timeout(cmd, Duration::from_millis(200)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }
    fn fixture(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "proc::tests::child_fixture", "--nocapture"])
            .env("XSES_PROCESS_TEST", mode);
        command
    }

    #[test]
    fn child_fixture() {
        match std::env::var("XSES_PROCESS_TEST").as_deref() {
            Ok("sleep") => std::thread::sleep(Duration::from_secs(30)),
            Ok("verbose") => {
                use std::io::Write;
                let chunk = vec![b'x'; 64 * 1024];
                for _ in 0..32 {
                    std::io::stdout().write_all(&chunk).unwrap();
                    std::io::stderr().write_all(&chunk).unwrap();
                }
            }
            _ => {}
        }
    }

    #[test]
    fn drains_both_pipes_before_the_child_exits() {
        let out = output_with_timeout(fixture("verbose"), Duration::from_secs(10)).unwrap();
        assert!(out.status.success());
        assert!(out.stdout.len() >= 2 * 1024 * 1024);
        assert_eq!(out.stderr.len(), 2 * 1024 * 1024);
    }
}
