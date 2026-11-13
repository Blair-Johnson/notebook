//! Child processes with a deadline: the UI thread never waits on one forever.

use std::io::{self, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub struct Finished {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Run `command` with `input` on stdin and collect its output, killing it
/// if it has not exited after `timeout` (an `ErrorKind::TimedOut` error).
///
/// Stdin is fed from its own thread and both outputs are read concurrently,
/// so a child that writes before it has read all its input cannot deadlock.
/// After a timeout the pipe threads are left to finish on their own: a
/// grandchild may still hold a pipe open.
pub fn run_with_timeout(
    command: &mut Command,
    input: &[u8],
    timeout: Duration,
) -> io::Result<Finished> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let input = input.to_vec();
        thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let Some(status) = wait_until(&mut child, Instant::now() + timeout)? else {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("timed out after {} s", timeout.as_secs_f32()),
        ));
    };
    Ok(Finished {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

/// Wait for `child` to exit; kill it at `deadline` and return `None`.
fn wait_until(child: &mut Child, deadline: Instant) -> io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feeds_input_and_collects_output() {
        // `cat` echoes while it reads: more than a pipe buffer each way.
        let input = vec![b'x'; 1 << 20];
        let done = run_with_timeout(&mut Command::new("cat"), &input, Duration::from_secs(10))
            .expect("cat runs");
        assert!(done.status.success());
        assert_eq!(done.stdout.len(), input.len());
    }

    #[test]
    fn kills_a_child_past_its_deadline() {
        let start = Instant::now();
        let mut command = Command::new("sleep");
        command.arg("30");
        let error = run_with_timeout(&mut command, b"", Duration::from_millis(200))
            .err()
            .expect("times out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
