use std::fmt;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// One error type for the crate. Callers print it; none branches on it.
#[derive(Debug)]
pub struct Error(pub String);

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error(format!("json: {e}"))
    }
}

const TAIL: usize = 2000;
// Time between SIGTERM to the group and the hard kill.
const GRACE: Duration = Duration::from_secs(5);
// Least time the pipes get to close once the child is gone, for a child that ends at the deadline.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Runs to completion and returns stdout. At `timeout` the child's whole process group gets
/// SIGTERM, then a kill after a grace period. Once the child exits its group is killed, and the
/// pipes must close by the deadline. Non-zero exit is Err with the stderr and stdout tails.
pub fn run(cmd: &mut Command, stdin: Option<&str>, timeout: Duration) -> Result<String> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    // Its own group, so a timeout reaches grandchildren (node workers, git hooks) too.
    std::os::unix::process::CommandExt::process_group(cmd, 0);
    let mut child = cmd
        .spawn()
        .map_err(|e| Error(format!("{name}: cannot start: {e}")))?;

    // Stdin is written and the pipes drained on their own threads: a prompt larger than the
    // pipe buffer would otherwise deadlock against a child blocked on writing its output.
    let writer = match (stdin, child.stdin.take()) {
        (Some(text), Some(mut pipe)) => {
            let text = text.to_owned();
            let (tx, rx) = mpsc::channel();
            // A child that exits early closes the pipe; its exit status reports the failure.
            thread::spawn(move || {
                let _ = pipe.write_all(text.as_bytes());
                let _ = tx.send(Vec::new());
            });
            Some(rx)
        }
        _ => None,
    };
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            terminate(&mut child);
            break None;
        }
        thread::sleep(Duration::from_millis(50));
    };
    // Background processes left in the group would hold the pipes open past the deadline.
    kill_group(&child);
    let by = deadline.max(Instant::now() + DRAIN_GRACE);
    let mut joined = Vec::new();
    for rx in writer.iter().chain([&out, &err]) {
        match rx.recv_timeout(by.saturating_duration_since(Instant::now())) {
            Ok(bytes) => joined.push(bytes),
            Err(RecvTimeoutError::Disconnected) => joined.push(Vec::new()),
            Err(RecvTimeoutError::Timeout) => {
                return Err(Error(format!(
                    "{name}: timed out after {}s: a process outside its group kept its pipes open",
                    timeout.as_secs_f32()
                )));
            }
        }
    }
    let stderr = String::from_utf8_lossy(&joined.pop().unwrap_or_default()).into_owned();
    let stdout = String::from_utf8_lossy(&joined.pop().unwrap_or_default()).into_owned();
    let failure = match status {
        Some(s) if s.success() => return Ok(stdout),
        Some(s) => format!("{name}: {s}"),
        None => format!("{name}: timed out after {}s", timeout.as_secs_f32()),
    };
    let mut msg = failure;
    for (label, text) in [("stderr", &stderr), ("stdout", &stdout)] {
        let text = tail(text.trim());
        if !text.is_empty() {
            msg.push_str(&format!("\n{label}: {text}"));
        }
    }
    Err(Error(msg))
}

/// SIGTERM to the group, a grace period, then a hard kill of the child; `run` kills the group.
fn terminate(child: &mut Child) {
    signal(child, "-TERM");
    let deadline = Instant::now() + GRACE;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn kill_group(child: &Child) {
    signal(child, "-KILL");
}

fn signal(child: &Child, sig: &str) {
    // By absolute path, so `PATH` cannot redirect it.
    let _ = Command::new("/bin/kill")
        .args([sig, "--", &format!("-{}", child.id())])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn tail(s: &str) -> &str {
    let mut start = s.len().saturating_sub(TAIL);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Reads the pipe to its end on a thread; the bytes arrive on the channel.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_stdout_and_feeds_stdin() {
        let big = "x".repeat(1 << 20);
        let out = run(&mut Command::new("wc"), Some(&big), Duration::from_secs(10)).unwrap();
        assert!(out.contains(&(1usize << 20).to_string()));
    }

    #[test]
    fn nonzero_exit_carries_stderr_and_stdout_tail() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo answer-detail; echo boom >&2; exit 3"]);
        let e = run(&mut cmd, None, Duration::from_secs(10)).unwrap_err();
        assert!(e.0.contains("boom"), "{e}");
        assert!(e.0.contains("answer-detail"), "{e}");
    }

    #[test]
    fn kills_at_timeout() {
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let e = run(&mut cmd, None, Duration::from_millis(300)).unwrap_err();
        assert!(e.0.contains("timed out"), "{e}");
    }

    #[test]
    fn a_background_child_holding_the_pipes_is_killed_at_exit() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "(sleep 6) & echo hi"]);
        let start = Instant::now();
        let out = run(&mut cmd, None, Duration::from_secs(1)).unwrap();
        assert_eq!(out.trim(), "hi");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn pipes_held_open_past_the_deadline_are_an_error() {
        // A process in its own session escapes the group kill and keeps stdout open. The shell
        // waits for its `setsid`, or the group kill at the shell's exit could still reach it.
        let dir = std::env::temp_dir().join(format!("dreamcatcher-setsid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = "perl -MPOSIX -e 'setsid(); open(F, \">ready\"); close(F); sleep 8' & \
                      while [ ! -f ready ]; do sleep 0.01; done; echo hi";
        let mut cmd = Command::new("sh");
        cmd.current_dir(&dir).args(["-c", script]);
        let start = Instant::now();
        let e = run(&mut cmd, None, Duration::from_millis(500)).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(e.0.contains("timed out"), "{e}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn timeout_terminates_the_whole_group_gracefully() {
        let dir = std::env::temp_dir().join(format!("dreamcatcher-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = "sleep 30 & echo $! > child.pid; \
                      trap 'echo bye > trapped; exit 0' TERM; \
                      while :; do sleep 0.1; done";
        let mut cmd = Command::new("sh");
        cmd.current_dir(&dir).args(["-c", script]);
        let e = run(&mut cmd, None, Duration::from_millis(500)).unwrap_err();
        assert!(e.0.contains("timed out"), "{e}");
        assert!(dir.join("trapped").exists(), "no SIGTERM reached the shell");
        let pid = std::fs::read_to_string(dir.join("child.pid")).unwrap();
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .status()
            .unwrap()
            .success();
        assert!(!alive, "the shell's child survived the timeout");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
