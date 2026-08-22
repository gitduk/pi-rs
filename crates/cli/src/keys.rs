//! Reading the keyboard while a run is in progress.
//!
//! rustyline owns the terminal only while it is reading a line. Between those
//! calls nothing is watching, which is why a running turn could be stopped by a
//! signal but not by a key.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// What a burst of raw input meant.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Input {
    /// A bare Escape — not the start of a sequence some key expands into.
    pub escape: bool,
    /// Printable characters, kept so what the user typed while waiting is not
    /// swallowed.
    pub text: String,
}

/// Read one burst of terminal bytes.
///
/// Escape is both a key and the prefix of most others: an arrow sends
/// `ESC [ A`. A lone `ESC` is the key; one followed by `[` or `O` opens a
/// sequence that runs to its final byte and means something else entirely.
pub fn interpret(bytes: &[u8]) -> Input {
    let mut out = Input::default();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => {
                match bytes.get(i + 1) {
                    Some(b'[') | Some(b'O') => {
                        // Runs to the first byte in the final range.
                        i += 2;
                        while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                            i += 1;
                        }
                        i += 1;
                    }
                    _ => {
                        out.escape = true;
                        i += 1;
                    }
                }
            }
            // Backspace, so a correction made while waiting is honoured.
            0x7f | 0x08 => {
                out.text.pop();
                i += 1;
            }
            b if (0x20..0x7f).contains(&b) => {
                out.text.push(b as char);
                i += 1;
            }
            // Everything else — newlines, Ctrl-anything — has nowhere to go
            // while there is no line being edited.
            _ => i += 1,
        }
    }
    out
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::io::IsTerminal;
    use std::os::fd::AsRawFd;

    /// The terminal settings to put back. Global because the exit path has to
    /// reach them too: `process::exit` runs no destructors, and a terminal left
    /// without ICANON is one the user has to `reset`.
    static SAVED: Mutex<Option<libc::termios>> = Mutex::new(None);

    pub fn restore() {
        let Ok(mut saved) = SAVED.lock() else { return };
        if let Some(t) = saved.take() {
            unsafe { libc::tcsetattr(std::io::stdin().as_raw_fd(), libc::TCSANOW, &t) };
        }
    }

    /// Keys arrive one at a time instead of one line at a time.
    ///
    /// ISIG stays on: Ctrl-C must keep reaching the signal handler that already
    /// stops a run.
    struct Raw;

    impl Raw {
        fn enter() -> Option<Self> {
            let fd = std::io::stdin().as_raw_fd();
            unsafe {
                let mut current: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(fd, &mut current) != 0 {
                    return None;
                }
                *SAVED.lock().ok()? = Some(current);
                let mut raw = current;
                raw.c_lflag &= !(libc::ICANON | libc::ECHO);
                // Return from read after 100ms with nothing typed, so the loop
                // notices the run has ended.
                raw.c_cc[libc::VMIN] = 0;
                raw.c_cc[libc::VTIME] = 1;
                if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                    return None;
                }
            }
            Some(Self)
        }
    }

    impl Drop for Raw {
        fn drop(&mut self) {
            restore();
        }
    }

    /// Watch the keyboard until `stop` is set. Escape cancels; anything else
    /// typed comes back for the next prompt.
    pub fn watch(
        cancel: tokio_util::sync::CancellationToken,
        stop: std::sync::Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<String> {
        tokio::task::spawn_blocking(move || {
            if !std::io::stdin().is_terminal() {
                return String::new();
            }
            let Some(_raw) = Raw::enter() else {
                return String::new();
            };
            let fd = std::io::stdin().as_raw_fd();
            let mut typed = String::new();
            let mut buf = [0u8; 256];
            while !stop.load(Ordering::Relaxed) {
                let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    continue;
                }
                let input = interpret(&buf[..n as usize]);
                if input.escape {
                    cancel.cancel();
                }
                typed.push_str(&input.text);
            }
            typed
        })
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    pub fn restore() {}

    pub fn watch(
        _cancel: tokio_util::sync::CancellationToken,
        _stop: std::sync::Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<String> {
        tokio::spawn(async { String::new() })
    }
}

pub use imp::{restore, watch};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_escape_is_the_key() {
        assert_eq!(
            interpret(b"\x1b"),
            Input {
                escape: true,
                text: String::new()
            }
        );
    }

    #[test]
    fn an_arrow_key_is_not_an_escape() {
        // Up, down, right, left, plus an SS3-form key.
        for seq in [&b"\x1b[A"[..], b"\x1b[B", b"\x1b[C", b"\x1b[D", b"\x1bOP"] {
            assert_eq!(interpret(seq), Input::default(), "{seq:?}");
        }
    }

    #[test]
    fn a_parameterized_sequence_is_consumed_whole() {
        // Home, and a bracketed-paste opener: neither leaves stray text behind.
        assert_eq!(interpret(b"\x1b[1;5H"), Input::default());
        assert_eq!(
            interpret(b"\x1b[200~hi"),
            Input {
                escape: false,
                text: "hi".into()
            }
        );
    }

    #[test]
    fn typing_while_waiting_is_kept() {
        assert_eq!(
            interpret(b"hello"),
            Input {
                escape: false,
                text: "hello".into()
            }
        );
        assert_eq!(
            interpret(b"ab\x7f c"),
            Input {
                escape: false,
                text: "a c".into()
            }
        );
    }

    #[test]
    fn escape_registers_even_amid_other_input() {
        let got = interpret(b"ab\x1bcd");
        assert!(got.escape);
        assert_eq!(got.text, "abcd");
    }

    #[test]
    fn newlines_and_control_bytes_go_nowhere() {
        // There is no line being edited to submit or interrupt.
        assert_eq!(interpret(b"\r\n\t\x03"), Input::default());
    }
}
