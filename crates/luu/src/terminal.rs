//! A command on a pseudo-terminal, for the page's terminal panel.
//!
//! What runs here is one of two lines, and `serve` decides which: a
//! `<runtime> exec -it <container> …` that
//! [`agent_core::worker::Worker::terminal_argv`] built, for a session in a
//! container, or this machine's own shell, for a session on the host. See
//! `RECORD/2026-10-01.a-terminal-in-the-container.completed.md` and
//! `RECORD/2026-10-01.the-terminal-follows-the-session.completed.md`.
//!
//! A PTY rather than pipes, because `exec -t` wants a TTY on its own side and
//! a shell without one has no line editing, no job control and no full-screen
//! programs. `libc::openpty` is the whole of it, which is why there is no
//! crate for it here.
//!
//! The master is read and written on two plain threads rather than through
//! the runtime: a PTY master is a blocking file, and a read that never returns
//! is the normal state of a terminal nobody is typing in.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use tokio::sync::mpsc;

/// One command on one PTY, for as long as the panel is open.
pub struct Pty {
    child: tokio::process::Child,
    /// Kept for resizing. The reader and the writer hold their own copies.
    master: File,
    /// What the command printed, in the chunks it was read in. Closed when the
    /// command's side of the PTY is: it exited, or it was hung up on.
    pub output: mpsc::Receiver<Vec<u8>>,
    input: std::sync::mpsc::Sender<Vec<u8>>,
}

impl Pty {
    /// Starts `argv` on a new PTY of `cols` × `rows`, in `cwd` where one is
    /// named. `TERM` is set for the command, because the page draws 256
    /// colours and an inherited `TERM` describes whatever started `serve`.
    pub fn spawn(
        argv: &[String],
        cwd: Option<&std::path::Path>,
        cols: u16,
        rows: u16,
    ) -> std::io::Result<Self> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| std::io::Error::other("an empty command line"))?;
        let (master, slave) = open(cols, rows)?;

        let mut command = tokio::process::Command::new(program);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command
            .args(args)
            .env("TERM", "xterm-256color")
            .stdin(File::from(slave.try_clone()?))
            .stdout(File::from(slave.try_clone()?))
            .stderr(File::from(slave))
            .kill_on_drop(true);
        // SAFETY: only async-signal-safe calls between fork and exec. A new
        // session, so the PTY can become its controlling terminal, and then
        // making it one: without that, ^C reaches nothing and a hangup is not
        // delivered when the panel closes.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // The slave's copies are the child's now; ours closes with `command`.
        let child = command.spawn()?;
        drop(command);

        let master = File::from(master);
        let (out_tx, output) = mpsc::channel(64);
        let mut reader = master.try_clone()?;
        std::thread::spawn(move || {
            let mut buffer = vec![0u8; 16 * 1024];
            loop {
                // `EIO` is how a master says the last holder of the slave
                // closed it, on Linux and macOS both: the same as end of file.
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if out_tx.blocking_send(buffer[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        let (input, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut writer = master.try_clone()?;
        std::thread::spawn(move || {
            while let Ok(bytes) = in_rx.recv() {
                if writer.write_all(&bytes).is_err() {
                    return;
                }
            }
        });

        Ok(Self {
            child,
            master,
            output,
            input,
        })
    }

    /// What was typed.
    pub fn write(&self, bytes: Vec<u8>) {
        // A writer that has gone is a command that has exited, and the output
        // closing is how the caller hears that.
        let _ = self.input.send(bytes);
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let size = winsize(cols, rows);
        // SAFETY: a valid fd we own and a `winsize` that outlives the call.
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &size);
        }
    }

    /// Ends the command the way closing a terminal window does: a hangup
    /// first, which `docker exec` passes on to the shell in the container, and
    /// a kill if it has not gone in two seconds.
    pub async fn hang_up(mut self) {
        if let Some(pid) = self.child.id() {
            // SAFETY: a signal to a process we started and have not reaped.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGHUP);
            }
        }
        if tokio::time::timeout(Duration::from_secs(2), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn open(cols: u16, rows: u16) -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut master = -1;
    let mut slave = -1;
    let mut size = winsize(cols, rows);
    // SAFETY: out-pointers to two ints, no name buffer, default termios, and a
    // `winsize` that outlives the call.
    let done = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if done < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `openpty` succeeded, so both are open fds nothing else owns.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // Not inherited by anything else this process starts: a worker that held
    // a copy of the master would keep the terminal open after the panel closed.
    for fd in [&master, &slave] {
        // SAFETY: a valid fd we own.
        unsafe {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    Ok((master, slave))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn until(pty: &mut Pty, wanted: &str) -> String {
        let mut seen = String::new();
        let found = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(chunk) = pty.output.recv().await {
                seen.push_str(&String::from_utf8_lossy(&chunk));
                if seen.contains(wanted) {
                    return true;
                }
            }
            false
        })
        .await;
        assert!(
            matches!(found, Ok(true)),
            "never saw {wanted:?} in {seen:?}"
        );
        seen
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[tokio::test]
    async fn a_command_on_a_pty_is_on_a_terminal_and_hears_what_is_typed() {
        let mut pty = Pty::spawn(
            &sh("test -t 0 && echo IS-A-TTY; stty size; read line; echo got:$line"),
            None,
            100,
            30,
        )
        .expect("spawned");
        until(&mut pty, "IS-A-TTY").await;
        // The size it was opened at is the size the command reads.
        until(&mut pty, "30 100").await;
        pty.write(b"hola\n".to_vec());
        until(&mut pty, "got:hola").await;
        // And then it exits, which closes the output.
        let rest = tokio::time::timeout(Duration::from_secs(5), pty.output.recv()).await;
        assert!(rest.is_ok(), "the output never closed");
    }

    #[tokio::test]
    async fn a_shell_opens_where_it_is_told_and_names_the_terminal_the_page_draws() {
        let dir = std::env::temp_dir().canonicalize().expect("a temp dir");
        let mut pty =
            Pty::spawn(&sh("echo AT=$(pwd -P) T=$TERM"), Some(&dir), 80, 24).expect("spawned");
        until(&mut pty, &format!("AT={} T=xterm-256color", dir.display())).await;
    }

    #[tokio::test]
    async fn a_resize_reaches_the_command() {
        let mut pty =
            Pty::spawn(&sh("read go; stty size; sleep 5"), None, 80, 24).expect("spawned");
        pty.resize(132, 50);
        pty.write(b"\n".to_vec());
        until(&mut pty, "50 132").await;
        pty.hang_up().await;
    }

    #[tokio::test]
    async fn hanging_up_ends_a_command_that_would_not_end_on_its_own() {
        let pty = Pty::spawn(&sh("sleep 600"), None, 80, 24).expect("spawned");
        let pid = pty.child.id().expect("running");
        let started = std::time::Instant::now();
        pty.hang_up().await;
        assert!(started.elapsed() < Duration::from_secs(3));
        // SAFETY: signal 0 only asks whether the pid exists.
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        assert!(!alive, "the command outlived its terminal");
    }
}
