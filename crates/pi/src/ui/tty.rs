//! What the terminal will say about itself.
//!
//! One question, asked once: xterm's `OSC 11`, which answers with the colour
//! the terminal paints behind everything. A band has to be a lift of that
//! colour to read as part of the terminal's own canvas rather than as a block
//! laid over it, and this is the only way to know what the canvas is.
//!
//! The answer is not a keystroke: the reply has no newline and no key that
//! names it, so it is read as bytes, before the keyboard reader starts — and
//! every byte of it goes, so none of it reaches the editor as text. What the
//! user typed while the question was out is read here too, and comes back with
//! the answer: nothing else would ever see it again.

#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(unix)]
use std::time::{Duration, Instant};

// How long a terminal that will not answer is given: one that has an answer
// gives it in a millisecond or two, and the wait is what the caller pays.
#[cfg(unix)]
const WAIT: Duration = Duration::from_millis(200);

// How long the rest of a reply or of a keystroke is given: either arrives in
// one piece, so this only covers a read split across the two.
#[cfg(unix)]
const TAIL: Duration = Duration::from_millis(20);

// A reply is tens of bytes, so a longer run is a terminal talking about
// something else: the attempt is dropped rather than followed.
#[cfg(unix)]
const REPLY_LIMIT: usize = 256;

// What a reply opens with, before the colour it carries — the same marker the
// query is written with. The bytes are matched against it one at a time.
#[cfg(unix)]
const OPENING: &str = "\x1b]11;";

/// What the terminal said about itself, and what asking it took from the user.
#[derive(Default)]
pub struct Answer {
    /// The terminal's own background, when it answered.
    pub bg: Option<(u8, u8, u8)>,
    /// What the user typed while the question was out. The asking read it, so
    /// it is handed back here — the keyboard reader would never see it again.
    pub typed: String,
}

/// The terminal's own background, when it answers.
///
/// Raw mode has to be up already: the reply ends in `BEL` or `ST`, never in a
/// newline, so a cooked read would wait for one that is not coming.
#[cfg(unix)]
pub fn background() -> Answer {
    let mut out = std::io::stdout();
    // The opening back with a `?` on it: what a terminal answers in.
    let ask = format!("{OPENING}?\x1b\\");
    if out.write_all(ask.as_bytes()).is_err() || out.flush().is_err() {
        // A terminal that cannot be written to has no answer either.
        return Answer::default();
    }
    read_answer(libc::STDIN_FILENO)
}

// Nothing is asked where nothing is read: a reply left in the input stream is
// a reply the editor would take as typed text.
#[cfg(not(unix))]
pub fn background() -> Answer {
    Answer::default()
}

// Read until the terminal answers or `WAIT` is up: a reply is taken, the user's
// own bytes are kept, and an attempt ends at its terminator or at `REPLY_LIMIT`.
#[cfg(unix)]
fn read_answer(fd: RawFd) -> Answer {
    let started = Instant::now();
    let mut asked = Answer::default();
    let mut held: Vec<u8> = Vec::new();
    let mut typed: Vec<u8> = Vec::new();
    let mut one = [0u8; 1];
    let opening = OPENING.as_bytes();
    loop {
        // A reply already in hand is waited out: its bytes are on their way,
        // and what is left of it must not be left for the keyboard reader.
        let left = match WAIT.checked_sub(started.elapsed()) {
            Some(left) => left,
            None if held.is_empty() => break,
            None => TAIL,
        };
        if !wait_on(fd, left) || read_bytes(fd, &mut one).is_none() {
            break;
        }
        let byte = one[0];
        // Text is what the user typed; the one byte a reply opens with is
        // held, and the next bytes say whether it was one.
        if held.is_empty() {
            if byte == opening[0] {
                held.push(byte);
            } else {
                typed.push(byte);
            }
            continue;
        }
        held.push(byte);
        if opening.starts_with(&held) {
            continue;
        }
        if !held.starts_with(opening) {
            // Not a reply after all: a keystroke's escape sequence is eaten to
            // its end, so no fragment of a key arrives later as a letter.
            let key = matches!(held.as_slice(), [0x1b, b'[' | b'O', ..]);
            held.clear();
            match byte {
                b'\x1b' => held.push(byte),
                _ if key => eat_key(fd),
                _ => typed.push(byte),
            }
            continue;
        }
        // The value runs to a terminator — a reply is a colour only once one
        // arrives — and an attempt open past `REPLY_LIMIT` was never one.
        if held.len() > REPLY_LIMIT {
            held.clear();
            continue;
        }
        if !matches!(byte, b'\x07' | 0x1b) {
            continue;
        }
        if let Some(bg) = parse(&held) {
            // A reply ended in `ST` is `ESC` and one more byte, and that byte
            // is not part of the colour: it must not be left behind either.
            if byte == 0x1b && wait_on(fd, TAIL) {
                let _ = read_bytes(fd, &mut one);
            }
            asked.bg = Some(bg);
            held.clear();
            break;
        }
        held.clear();
    }
    asked.typed = text_of(&typed);
    asked
}

// The rest of a keystroke that opened with `ESC [ ` or `ESC O`, final byte
// included. `ESC [ M` is an X10 mouse report, whose three bytes follow it.
#[cfg(unix)]
fn eat_key(fd: RawFd) {
    let mut one = [0u8; 1];
    let mut first = true;
    while wait_on(fd, TAIL) && read_bytes(fd, &mut one).is_some() {
        if (0x40..=0x7e).contains(&one[0]) {
            let mouse = first && one[0] == b'M';
            for _ in 0..if mouse { 3 } else { 0 } {
                if !wait_on(fd, TAIL) || read_bytes(fd, &mut one).is_none() {
                    return;
                }
            }
            return;
        }
        first = false;
    }
}

// What is waiting in the terminal, if anything: `poll` has just said there is
// a byte, so this cannot park.
#[cfg(unix)]
fn read_bytes(fd: RawFd, buf: &mut [u8]) -> Option<usize> {
    // SAFETY: reading into a buffer this frame owns, capped at its length.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    (n > 0).then_some(n as usize)
}

// Whether the terminal has anything for us within `within`. Nothing to wait
// for is the common answer: most terminals keep quiet about this.
#[cfg(unix)]
fn wait_on(fd: RawFd, within: Duration) -> bool {
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one `pollfd` owned here, and a timeout in milliseconds.
    unsafe { libc::poll(&mut poll_fd, 1, within.as_millis() as libc::c_int) > 0 }
}

// The user's own bytes as the text they meant: a control character is a key,
// not text, and a paste keeps the newlines it was copied with.
#[cfg(unix)]
fn text_of(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect()
}

/// The background an answer names, if one is in there.
///
/// Found rather than matched whole: the answer arrives alone, or behind the
/// query a terminal read back to us, and both carry the marker and a colour.
/// The forms a terminal answers in are `rgb:RRRR/GGGG/BBBB` and `#rrggbb`; the
/// high byte of each component is the colour.
#[cfg(unix)]
fn parse(bytes: &[u8]) -> Option<(u8, u8, u8)> {
    let text = String::from_utf8_lossy(bytes);
    // Every marker is tried: the query itself may be in front of the answer,
    // and only one of them carries a colour.
    text.match_indices(OPENING)
        .find_map(|(at, _)| colour(&text[at + OPENING.len()..]))
}

// The colour one answer names, as the rest of the buffer from its marker.
#[cfg(unix)]
fn colour(value: &str) -> Option<(u8, u8, u8)> {
    // The value is what a terminator ends: one still being written is a
    // partial read, and `rgb:.../1` is only a component once it is closed.
    let value = &value[..value.find(['\x07', '\x1b'])?];
    let value = value.trim_start_matches("rgb:").trim_start_matches("rgba:");
    if let Some(hex) = value.trim().strip_prefix('#') {
        // X11 writes a component in as many digits as it likes: `#f0f` and
        // `#0d0d11111717` name the same colour as `#0d1117`.
        if hex.is_empty() || hex.len() % 3 != 0 {
            return None;
        }
        let per = hex.len() / 3;
        return Some((
            component(hex.get(..per)?)?,
            component(hex.get(per..per * 2)?)?,
            component(hex.get(per * 2..)?)?,
        ));
    }
    let mut parts = value.split(['/', ':']);
    Some((
        component(parts.next()?)?,
        component(parts.next()?)?,
        component(parts.next()?)?,
    ))
}

// One component of a colour, however many hex digits it was written with.
#[cfg(unix)]
fn component(part: &str) -> Option<u8> {
    let part = part.trim();
    match part.len() {
        0 => None,
        1 => u8::from_str_radix(&format!("{part}{part}"), 16).ok(),
        _ => u8::from_str_radix(part.get(..2)?, 16).ok(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    // The shapes an answer comes in, from the terminals that send them: foot's
    // 16-bit components, an 8-bit one, and X11's `#` forms, each terminated.
    #[test]
    fn an_answer_is_read_however_it_is_written() {
        for (reply, want) in [
            ("\x1b]11;rgb:0d0d/1111/1717\x07", (0x0d, 0x11, 0x17)),
            ("\x1b]11;rgb:0d/11/17\x1b\\", (0x0d, 0x11, 0x17)),
            ("\x1b]11;#0d1117\x07", (0x0d, 0x11, 0x17)),
            ("\x1b]11;#0d0d11111717\x07", (0x0d, 0x11, 0x17)),
            // A lone digit means the colour, written short.
            ("\x1b]11;rgb:f/f/f\x07", (0xff, 0xff, 0xff)),
            ("\x1b]11;#f0f\x07", (0xff, 0x00, 0xff)),
        ] {
            assert_eq!(parse(reply.as_bytes()), Some(want), "for {reply:?}");
        }
    }

    // A terminal that read the question back to us puts it in front of the
    // answer: the answer behind it is still the answer.
    #[test]
    fn a_query_in_front_of_the_answer_is_not_a_colour() {
        let both = "\x1b]11;?\x1b\\\x1b]11;rgb:0d0d/1111/1717\x07";
        assert_eq!(parse(both.as_bytes()), Some((0x0d, 0x11, 0x17)));
        assert_eq!(parse(b"\x1b]11;?\x1b\\"), None);
    }

    // Half an answer is no answer: a partial read is what the next poll will
    // complete, and a colour guessed from `rg` is a colour nobody asked for.
    #[test]
    fn a_partial_answer_is_not_a_colour() {
        for partial in [
            "",
            "\x1b]11;",
            "\x1b]11;rgb:0d0d/1111",
            "\x1b]11;rgb:",
            "\x1b]11;rgb:0d0d/1111/1",
            "\x1b]11;#0d111",
            "\x1b]11;#0d1",
        ] {
            assert_eq!(parse(partial.as_bytes()), None, "for {partial:?}");
        }
        // What a keystroke looks like on the way past: a word, not a colour.
        assert_eq!(parse(b"fix the bug\n"), None);
    }

    // What the wait keeps of the user's bytes: text, whole, and no key in it.
    #[test]
    fn what_the_user_typed_comes_back_as_text() {
        assert_eq!(text_of(b"look at src/ui.rs"), "look at src/ui.rs");
        assert_eq!(text_of("なにか書く".as_bytes()), "なにか書く");
        assert_eq!(text_of(b"one\ntwo\n"), "one\ntwo\n");
        // A control byte is a key: Enter, Tab, Ctrl-C, Backspace, Escape.
        assert_eq!(text_of(b"a\rb\tc\x03d\x7fe\x1bf"), "abcdef");
    }

    // A pipe standing in for the terminal. What is left over after the read is
    // what the keyboard reader would have taken.
    fn answer(sent: &[u8]) -> (Answer, String) {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: two fds into an array this frame owns.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: writing what the caller gave, then closing the write end so
        // that the read end sees the end of the stream.
        let written = unsafe { libc::write(fds[1], sent.as_ptr().cast(), sent.len()) };
        assert_eq!(written as usize, sent.len());
        unsafe { libc::close(fds[1]) };
        let asked = read_answer(fds[0]);
        let mut left = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            // SAFETY: reading into a buffer this frame owns, capped at its
            // length; the write end is closed, so this ends rather than parking.
            let n = unsafe { libc::read(fds[0], buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            left.extend_from_slice(&buf[..n as usize]);
        }
        // SAFETY: the fd was opened here, and this is its last use.
        unsafe { libc::close(fds[0]) };
        (asked, String::from_utf8_lossy(&left).into_owned())
    }

    // The user pressed Escape while the question was out: the reply behind it
    // is still a reply, and none of it may come back as text.
    #[test]
    fn an_escape_before_the_answer_does_not_spill_it() {
        let (asked, left) = answer(b"\x1b\x1b]11;rgb:0d0d/1111/1717\x1b\\");
        assert_eq!(asked.bg, Some((0x0d, 0x11, 0x17)));
        assert_eq!(asked.typed, "");
        assert_eq!(left, "");
    }

    // Typing before the answer is read with it, and typing after it is left
    // for the keyboard reader — the answer is where the asking ends.
    #[test]
    fn what_is_typed_around_the_answer_is_kept() {
        let (asked, left) = answer(b"ls\x1b]11;#0d1117\x07 -a");
        assert_eq!(asked.bg, Some((0x0d, 0x11, 0x17)));
        assert_eq!(asked.typed, "ls");
        assert_eq!(left, " -a");
        // Escape then typing: the byte after the Escape is not a key's.
        assert_eq!(answer(b"\x1bh\x07").0.typed, "h");
    }

    // A key is not text: neither its own bytes nor the payload an X10 mouse
    // report carries after its final byte.
    #[test]
    fn a_key_leaves_nothing_of_itself_behind() {
        assert_eq!(answer(b"\x1b[Chello").0.typed, "hello");
        assert_eq!(answer(b"\x1b[M !\"hello").0.typed, "hello");
    }

    // An answer that ends without a colour is not an answer, and it must not
    // swallow what comes after it either.
    #[test]
    fn a_broken_answer_does_not_take_the_next_bytes_with_it() {
        let (asked, left) = answer(b"\x1b]11;rgb:x0d0d/1111/1717\x07hello");
        assert_eq!(asked.bg, None);
        assert_eq!(asked.typed, "hello");
        assert_eq!(left, "");
    }
}
