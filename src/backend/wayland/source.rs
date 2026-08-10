use std::{
    fs::File,
    io::Write,
    os::fd::AsFd,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use nix::poll::{PollFd, PollFlags, poll};

use crate::domain::{TextPayload, TransferError};

use super::offer::{IDLE_TIMEOUT, TOTAL_TIMEOUT};

const IO_POLL_MS: u16 = 100;

pub(super) fn write_pipe(
    fd: std::os::fd::OwnedFd,
    payload: &TextPayload,
    cancelled: &AtomicBool,
) -> Result<(), TransferError> {
    write_pipe_with_limits(
        fd,
        payload,
        cancelled,
        IDLE_TIMEOUT,
        TOTAL_TIMEOUT,
        IO_POLL_MS,
    )
}

fn write_pipe_with_limits(
    fd: std::os::fd::OwnedFd,
    payload: &TextPayload,
    cancelled: &AtomicBool,
    idle_timeout: Duration,
    total_timeout: Duration,
    poll_timeout_ms: u16,
) -> Result<(), TransferError> {
    let mut file = File::from(fd);
    super::set_nonblocking(file.as_fd(), "set-write-pipe-nonblocking")?;
    let bytes = payload.as_str().as_bytes();
    let started = Instant::now();
    let mut last_progress = started;
    let mut offset = 0;

    while offset < bytes.len() {
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
            PollFlags::POLLOUT | PollFlags::POLLHUP | PollFlags::POLLERR,
        )];
        match poll(&mut poll_fds, poll_timeout_ms) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(error) => return Err(TransferError::io("poll-write-pipe", error)),
        }
        match file.write(&bytes[offset..]) {
            Ok(0) => {
                return Err(TransferError::io(
                    "write-pipe",
                    "write returned zero before payload completed",
                ));
            }
            Ok(written) => {
                offset += written;
                last_progress = Instant::now();
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(TransferError::io("write-pipe", error)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{io::Read, thread, time::Duration};

    use super::*;

    #[test]
    fn pipe_writer_sends_the_entire_payload() {
        let text = "partial-write-safe ☃".repeat(16_384);
        let payload = TextPayload::from_string(text.clone()).expect("test payload is valid");
        let (read_fd, write_fd) = nix::unistd::pipe().expect("create test pipe");
        let reader = thread::spawn(move || {
            let mut file = File::from(read_fd);
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).expect("read test pipe");
            bytes
        });
        let cancelled = AtomicBool::new(false);
        write_pipe(write_fd, &payload, &cancelled).expect("write complete test payload");
        let bytes = reader.join().expect("test reader does not panic");
        assert_eq!(bytes, text.as_bytes());
    }

    #[test]
    fn pipe_writer_honors_cancellation() {
        let payload = TextPayload::from_string("cancelled".to_owned()).expect("valid test text");
        let (_read_fd, write_fd) = nix::unistd::pipe().expect("create test pipe");
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            write_pipe(write_fd, &payload, &cancelled),
            Err(TransferError::Cancelled)
        );
    }

    #[test]
    fn pipe_writer_reports_idle_timeout_when_requester_stops_reading() {
        let payload = TextPayload::from_string("x".repeat(1024 * 1024))
            .expect("test payload is within the size limit");
        let (read_fd, write_fd) = nix::unistd::pipe().expect("create idle test pipe");
        let cancelled = AtomicBool::new(false);
        assert_eq!(
            write_pipe_with_limits(
                write_fd,
                &payload,
                &cancelled,
                Duration::from_millis(20),
                Duration::from_secs(1),
                5,
            ),
            Err(TransferError::IdleTimeout)
        );
        drop(read_fd);
    }

    #[test]
    fn pipe_writer_reports_total_timeout_despite_slow_progress() {
        let payload = TextPayload::from_string("x".repeat(1024 * 1024))
            .expect("test payload is within the size limit");
        let (read_fd, write_fd) = nix::unistd::pipe().expect("create total-timeout test pipe");
        let reader = thread::spawn(move || {
            let mut file = File::from(read_fd);
            let mut chunk = [0_u8; 4096];
            loop {
                match file.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        let cancelled = AtomicBool::new(false);
        assert_eq!(
            write_pipe_with_limits(
                write_fd,
                &payload,
                &cancelled,
                Duration::from_millis(30),
                Duration::from_millis(80),
                5,
            ),
            Err(TransferError::TotalTimeout)
        );
        reader.join().expect("slow reader does not panic");
    }
}
