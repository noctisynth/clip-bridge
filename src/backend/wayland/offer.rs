use std::{
    fs::File,
    io::Read,
    os::fd::AsFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use nix::poll::{PollFd, PollFlags, poll};

use crate::domain::{MAX_TEXT_BYTES, TextPayload, TransferError};

pub(super) const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const IO_POLL_MS: u16 = 100;

pub(super) fn choose_mime(mime_types: &[String]) -> Option<String> {
    mime_types
        .iter()
        .find(|mime| mime.as_str() == "text/plain;charset=utf-8")
        .cloned()
        .or_else(|| {
            mime_types
                .iter()
                .find(|mime| mime_is_utf8_text(mime))
                .cloned()
        })
        .or_else(|| {
            mime_types
                .iter()
                .find(|mime| mime_is_plain_text_without_charset(mime))
                .cloned()
        })
}

fn mime_is_utf8_text(mime: &str) -> bool {
    let mut parts = mime.split(';');
    if !parts
        .next()
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("text/plain"))
    {
        return false;
    }
    parts.any(|parameter| {
        parameter.split_once('=').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("charset")
                && value.trim().trim_matches('"').eq_ignore_ascii_case("utf-8")
        })
    })
}

fn mime_is_plain_text_without_charset(mime: &str) -> bool {
    let mut parts = mime.split(';');
    parts
        .next()
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("text/plain"))
        && !parts.any(|parameter| {
            parameter
                .split_once('=')
                .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("charset"))
        })
}

pub(super) fn read_pipe(
    fd: std::os::fd::OwnedFd,
    cancelled: &AtomicBool,
) -> Result<TextPayload, TransferError> {
    read_pipe_with_limits(fd, cancelled, IDLE_TIMEOUT, TOTAL_TIMEOUT, IO_POLL_MS)
}

fn read_pipe_with_limits(
    fd: std::os::fd::OwnedFd,
    cancelled: &AtomicBool,
    idle_timeout: Duration,
    total_timeout: Duration,
    poll_timeout_ms: u16,
) -> Result<TextPayload, TransferError> {
    let mut file = File::from(fd);
    super::set_nonblocking(file.as_fd(), "set-read-pipe-nonblocking")?;
    let started = Instant::now();
    let mut last_progress = started;
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];

    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(TransferError::Cancelled);
        }
        let now = Instant::now();
        if now.duration_since(started) >= total_timeout {
            return Err(TransferError::TotalTimeout);
        }
        if now.duration_since(last_progress) >= idle_timeout {
            return Err(TransferError::IdleTimeout);
        }

        let mut poll_fds = [PollFd::new(
            file.as_fd(),
            PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
        )];
        match poll(&mut poll_fds, poll_timeout_ms) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(error) => return Err(TransferError::io("poll-read-pipe", error)),
        }

        match file.read(&mut chunk) {
            Ok(0) => return TextPayload::from_bytes(buffer).map_err(payload_error),
            Ok(read) => {
                let size = buffer.len().saturating_add(read);
                if size > MAX_TEXT_BYTES {
                    return Err(TransferError::TooLarge {
                        size,
                        max: MAX_TEXT_BYTES,
                    });
                }
                buffer.extend_from_slice(&chunk[..read]);
                last_progress = Instant::now();
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(TransferError::io("read-pipe", error)),
        }
    }
}

fn payload_error(error: crate::domain::TextPayloadError) -> TransferError {
    match error {
        crate::domain::TextPayloadError::Empty => TransferError::Empty,
        crate::domain::TextPayloadError::InvalidUtf8 => TransferError::InvalidUtf8,
        crate::domain::TextPayloadError::TooLarge { size, max } => {
            TransferError::TooLarge { size, max }
        }
    }
}

pub(super) fn cancellation() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[cfg(test)]
mod tests {
    use std::{io::Write, thread};

    use super::*;

    #[test]
    fn mime_selection_obeys_utf8_priority() {
        let offered = vec![
            "text/plain".to_owned(),
            "TEXT/PLAIN; format=flowed; CHARSET=\"UTF-8\"".to_owned(),
            "text/plain;charset=utf-8".to_owned(),
        ];
        assert_eq!(
            choose_mime(&offered).as_deref(),
            Some("text/plain;charset=utf-8")
        );
    }

    #[test]
    fn unsupported_charset_is_not_treated_as_utf8() {
        let offered = vec!["text/plain;charset=iso-8859-1".to_owned()];
        assert_eq!(choose_mime(&offered), None);
    }

    #[test]
    fn pipe_reader_collects_complete_utf8_payload() {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("create test pipe");
        let writer = thread::spawn(move || {
            let mut file = File::from(write_fd);
            file.write_all("complete pipe payload ☃".as_bytes())
                .expect("write test payload");
        });
        let cancelled = AtomicBool::new(false);
        let payload = read_pipe(read_fd, &cancelled).expect("read complete test payload");
        writer.join().expect("test writer does not panic");
        assert_eq!(payload.as_str(), "complete pipe payload ☃");
    }

    #[test]
    fn pipe_reader_rejects_invalid_utf8() {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("create test pipe");
        let mut file = File::from(write_fd);
        file.write_all(&[0xff]).expect("write invalid UTF-8");
        drop(file);
        let cancelled = AtomicBool::new(false);
        assert_eq!(
            read_pipe(read_fd, &cancelled),
            Err(TransferError::InvalidUtf8)
        );
    }

    #[test]
    fn pipe_reader_rejects_oversized_payload() {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("create test pipe");
        let writer = thread::spawn(move || {
            let mut file = File::from(write_fd);
            let result = file.write_all(&vec![b'x'; MAX_TEXT_BYTES + 1]);
            if let Err(error) = result {
                assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
            }
        });
        let cancelled = AtomicBool::new(false);
        assert!(matches!(
            read_pipe(read_fd, &cancelled),
            Err(TransferError::TooLarge { max, .. }) if max == MAX_TEXT_BYTES
        ));
        writer.join().expect("oversized writer does not panic");
    }

    #[test]
    fn pipe_reader_reports_idle_timeout_and_cancellation() {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("create idle test pipe");
        let cancelled = AtomicBool::new(false);
        assert_eq!(
            read_pipe_with_limits(
                read_fd,
                &cancelled,
                Duration::from_millis(20),
                Duration::from_secs(1),
                5,
            ),
            Err(TransferError::IdleTimeout)
        );
        drop(write_fd);

        let (read_fd, _write_fd) = nix::unistd::pipe().expect("create cancelled test pipe");
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            read_pipe_with_limits(
                read_fd,
                &cancelled,
                Duration::from_secs(1),
                Duration::from_secs(1),
                5,
            ),
            Err(TransferError::Cancelled)
        );
    }

    #[test]
    fn pipe_reader_reports_total_timeout_despite_progress() {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("create total-timeout test pipe");
        let writer = thread::spawn(move || {
            let mut file = File::from(write_fd);
            loop {
                if let Err(error) = file.write_all(b"x") {
                    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        let cancelled = AtomicBool::new(false);
        assert_eq!(
            read_pipe_with_limits(
                read_fd,
                &cancelled,
                Duration::from_millis(20),
                Duration::from_millis(50),
                5,
            ),
            Err(TransferError::TotalTimeout)
        );
        writer.join().expect("total-timeout writer does not panic");
    }
}
