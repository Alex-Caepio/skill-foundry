//! Bounded subprocess execution with streaming JSON-line event observation.
use crate::{Result, RuntimeError, cancel, common};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

pub struct Capture {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub stopped: String,
    pub utf8_valid: bool,
}

pub fn invoke(
    argv: Vec<String>,
    cwd: &Path,
    env: BTreeMap<String, String>,
    input: Vec<u8>,
    timeout: Duration,
    mut observer: impl FnMut(&Value) -> Option<String> + Send,
) -> Result<Capture> {
    if argv.is_empty() {
        return Err(RuntimeError::invalid("Empty command argv"));
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .env_clear()
        .envs(&env)
        .stdin(if input.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| RuntimeError::unavailable(format!("Failed to start {}: {e}", argv[0])))?;

    if !input.is_empty()
        && let Some(mut stdin) = child.stdin.take()
    {
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| RuntimeError::unavailable("Child stdout missing"))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| RuntimeError::unavailable("Child stderr missing"))?;

    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.take(MAX_OUTPUT_BYTES as u64).read_to_end(&mut buf);
        buf
    });

    let (line_tx, line_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let out_handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout_pipe.take(MAX_OUTPUT_BYTES as u64));
        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if line_tx.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let deadline = Instant::now() + timeout;
    let mut raw_stdout = Vec::new();
    let mut stopped = "finished".to_owned();
    let mut exit_code = None;

    loop {
        if cancel::interrupted() {
            stopped = "interrupted".into();
            let _ = child.kill();
            break;
        }
        if Instant::now() >= deadline {
            stopped = "timeout".into();
            let _ = child.kill();
            break;
        }
        match line_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                if raw_stdout.len() + line.len() <= MAX_OUTPUT_BYTES {
                    raw_stdout.extend_from_slice(&line);
                }
                if let Ok(val) = serde_json::from_slice::<Value>(&line)
                    && val.is_object()
                    && let Some(reason) = observer(&val)
                {
                    stopped = reason;
                    let _ = child.kill();
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(Some(status)) = child.try_wait() {
                    exit_code = status.code();
                    while let Ok(line) = line_rx.try_recv() {
                        if raw_stdout.len() + line.len() <= MAX_OUTPUT_BYTES {
                            raw_stdout.extend_from_slice(&line);
                        }
                        if let Ok(val) = serde_json::from_slice::<Value>(&line)
                            && val.is_object()
                            && let Some(reason) = observer(&val)
                        {
                            stopped = reason;
                        }
                    }
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    if exit_code.is_none() {
        exit_code = child.wait().ok().and_then(|s| s.code());
    }
    let _ = out_handle.join();
    let raw_stderr = err_handle.join().unwrap_or_default();
    let utf8_valid =
        std::str::from_utf8(&raw_stdout).is_ok() && std::str::from_utf8(&raw_stderr).is_ok();

    Ok(Capture {
        code: exit_code.unwrap_or(-1),
        stdout: common::normalize(&String::from_utf8_lossy(&raw_stdout)),
        stderr: common::normalize(&String::from_utf8_lossy(&raw_stderr)),
        stopped,
        utf8_valid,
    })
}

pub fn success(output: Capture, label: &str) -> Result<String> {
    if !output.utf8_valid {
        return Err(RuntimeError::invalid(format!(
            "{label} returned invalid UTF-8"
        )));
    }
    let early = output.stopped == "probe-observed" || output.stopped == "routing-observed";
    if !early && (output.stopped != "finished" || output.code != 0) {
        return Err(RuntimeError::unavailable(format!(
            "{label} exited {} ({}): {}",
            output.code,
            output.stopped,
            output.stderr.trim()
        )));
    }
    Ok(output.stdout)
}
