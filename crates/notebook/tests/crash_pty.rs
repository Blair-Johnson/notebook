#![allow(unsafe_code)]

//! PTY teardown coverage for signals and terminal-mode restoration.

#[cfg(unix)]
mod unix_pty {
    use std::fs;
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn read_available(pty: &mut fs::File, output: &mut Vec<u8>, deadline: Instant) {
        let mut buffer = [0u8; 65_536];
        while Instant::now() < deadline {
            match pty.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => output.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(_) => break,
            }
        }
    }

    fn read_until_sequence(
        pty: &mut fs::File,
        output: &mut Vec<u8>,
        sequence: &[u8],
        deadline: Instant,
    ) {
        let mut buffer = [0u8; 65_536];
        while Instant::now() < deadline {
            match pty.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    output.extend_from_slice(&buffer[..count]);
                    if output
                        .windows(sequence.len())
                        .any(|window| window == sequence)
                    {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(_) => break,
            }
        }
    }

    fn temp_root() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "nb-pty-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ))
    }

    #[test]
    fn sigterm_restores_alternate_screen_and_bracketed_paste() {
        let root = temp_root();
        let state_dir = root.join("state");
        let config_path = root.join("config.toml");
        let notebook = root.join("session.py");
        fs::create_dir_all(&state_dir).expect("create state directory");
        fs::write(&notebook, "# %%\nvalue = 1\n").expect("write notebook");

        let mut master = 0;
        let mut slave = 0;
        let window = libc::winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let result = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(result, 0, "openpty failed");
        assert_eq!(
            unsafe { libc::ioctl(master, libc::TIOCSWINSZ as _, &window) },
            0,
            "set PTY size",
        );

        let mut command = Command::new(env!("CARGO_BIN_EXE_nb"));
        command
            .arg(&notebook)
            .env("NB_CONFIG", &config_path)
            .env("NB_STATE_DIR", &state_dir)
            .env("TERM", "xterm-256color")
            .env("RUST_BACKTRACE", "0");
        unsafe {
            let stdin_fd = libc::dup(slave);
            let stdout_fd = libc::dup(slave);
            let stderr_fd = libc::dup(slave);
            assert!(stdin_fd >= 0 && stdout_fd >= 0 && stderr_fd >= 0);
            command
                .stdin(Stdio::from_raw_fd(stdin_fd))
                .stdout(Stdio::from_raw_fd(stdout_fd))
                .stderr(Stdio::from_raw_fd(stderr_fd));
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(slave, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command.spawn().expect("spawn nb");
        unsafe {
            libc::close(slave);
            let flags = libc::fcntl(master, libc::F_GETFL);
            assert!(flags >= 0, "fcntl get flags failed");
            assert!(libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0);
        }
        let mut pty = unsafe { fs::File::from_raw_fd(master) };

        let mut output = Vec::new();
        read_until_sequence(
            &mut pty,
            &mut output,
            b"\x1b[?1049h",
            Instant::now() + Duration::from_secs(15),
        );
        assert!(
            output
                .windows(b"\x1b[?1049h".len())
                .any(|window| window == b"\x1b[?1049h"),
            "nb never entered the alternate screen: {}",
            String::from_utf8_lossy(&output)
        );

        let child_pid = child.id() as libc::pid_t;
        assert_eq!(unsafe { libc::kill(child_pid, libc::SIGTERM) }, 0);
        let exit_deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            read_available(
                &mut pty,
                &mut output,
                Instant::now() + Duration::from_millis(100),
            );
            if let Some(status) = child.try_wait().expect("poll nb") {
                break status;
            }
            if Instant::now() >= exit_deadline {
                let _ = unsafe { libc::kill(child_pid, libc::SIGKILL) };
                panic!("nb did not exit after SIGTERM");
            }
        };
        read_available(
            &mut pty,
            &mut output,
            Instant::now() + Duration::from_millis(500),
        );

        assert!(
            status.success(),
            "nb failed after SIGTERM: {}\n{}",
            status,
            String::from_utf8_lossy(&output)
        );
        let tail_start = output
            .windows(b"\x1b[?1049h".len())
            .rposition(|window| window == b"\x1b[?1049h")
            .unwrap_or(0);
        let tail = String::from_utf8_lossy(&output[tail_start..]);
        assert!(
            tail.contains("\x1b[?1049l"),
            "alternate screen not restored: {tail:?}"
        );
        assert!(
            tail.contains("\x1b[?2004l"),
            "bracketed paste not disabled: {tail:?}"
        );
        assert!(
            tail.contains("\x1b[?1000l"),
            "mouse mode not disabled: {tail:?}"
        );

        let _ = fs::remove_dir_all(root);
        pty.flush().ok();
    }

    #[test]
    fn panic_restores_terminal_and_writes_autosave_sidecar() {
        let root = temp_root();
        let state_dir = root.join("state");
        let config_path = root.join("config.toml");
        let notebook = root.join("panic_session.py");
        fs::create_dir_all(&state_dir).expect("create state directory");
        fs::write(&notebook, "# %%\nvalue = 1\n").expect("write notebook");

        let mut master = 0;
        let mut slave = 0;
        let window = libc::winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        assert_eq!(
            unsafe { libc::ioctl(master, libc::TIOCSWINSZ as _, &window) },
            0
        );

        let mut command = Command::new(env!("CARGO_BIN_EXE_nb"));
        command
            .arg(&notebook)
            .env("NB_CONFIG", &config_path)
            .env("NB_STATE_DIR", &state_dir)
            .env("TERM", "xterm-256color")
            .env("RUST_BACKTRACE", "0");
        unsafe {
            let stdin_fd = libc::dup(slave);
            let stdout_fd = libc::dup(slave);
            let stderr_fd = libc::dup(slave);
            assert!(stdin_fd >= 0 && stdout_fd >= 0 && stderr_fd >= 0);
            command
                .stdin(Stdio::from_raw_fd(stdin_fd))
                .stdout(Stdio::from_raw_fd(stdout_fd))
                .stderr(Stdio::from_raw_fd(stderr_fd));
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(slave, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command.spawn().expect("spawn nb");
        unsafe {
            libc::close(slave);
            let flags = libc::fcntl(master, libc::F_GETFL);
            assert!(flags >= 0);
            assert!(libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0);
        }
        let mut pty = unsafe { fs::File::from_raw_fd(master) };

        let mut output = Vec::new();
        read_until_sequence(
            &mut pty,
            &mut output,
            b"\x1b[?1049h",
            Instant::now() + Duration::from_secs(15),
        );
        read_available(&mut pty, &mut output, Instant::now() + Duration::from_millis(100));

        pty.write_all(b"A + 41").expect("type cell edit");
        pty.flush().ok();
        std::thread::sleep(Duration::from_millis(60));
        pty.write_all(b"\x1b").expect("leave insert mode");
        pty.flush().ok();
        std::thread::sleep(Duration::from_millis(60));
        pty.write_all(b"\x1b").expect("leave cell normal mode");
        pty.flush().ok();
        std::thread::sleep(Duration::from_millis(60));
        pty.write_all(b":panic\r").expect("trigger intentional panic");
        pty.flush().ok();

        let exit_deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            read_available(
                &mut pty,
                &mut output,
                Instant::now() + Duration::from_millis(100),
            );
            if let Some(status) = child.try_wait().expect("poll nb") {
                break status;
            }
            if Instant::now() >= exit_deadline {
                let _ = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
                panic!(
                    "nb did not exit after :panic:\n{}",
                    String::from_utf8_lossy(&output)
                );
            }
        };
        read_available(
            &mut pty,
            &mut output,
            Instant::now() + Duration::from_millis(300),
        );

        assert!(!status.success(), "expected non-zero panic exit, got {status}");
        let tail_start = output
            .windows(b"\x1b[?1049h".len())
            .rposition(|window| window == b"\x1b[?1049h")
            .unwrap_or(0);
        let tail = String::from_utf8_lossy(&output[tail_start..]);
        assert!(tail.contains("\x1b[?1049l"), "alternate screen not restored: {tail:?}");
        assert!(tail.contains("\x1b[?2004l"), "bracketed paste not disabled: {tail:?}");
        assert!(tail.contains("\x1b[?1000l"), "mouse mode not disabled: {tail:?}");

        let autosave_dir = state_dir.join("autosave");
        let entries: Vec<_> = fs::read_dir(&autosave_dir)
            .expect("autosave directory created")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one autosave sidecar");
        let sidecar_raw = fs::read_to_string(entries[0].path()).expect("read sidecar");
        let recovered = nb_core::codec::ipynb::parse_ipynb(&sidecar_raw).expect("parse sidecar");
        let first_id = &recovered.order()[0];
        let recovered_src = recovered.cell(first_id).expect("first cell").buffer.as_str();
        assert!(
            recovered_src.contains("value = 1 + 41"),
            "autosave sidecar missing edited source: {recovered_src:?}"
        );
        let _ = fs::remove_dir_all(root);
        pty.flush().ok();
    }
}

#[cfg(not(unix))]
#[test]
fn pty_teardown_is_unix_only() {}
