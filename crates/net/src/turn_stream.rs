//! Bounded TURN stream output, independent of the socket/TLS readiness driver.
//!
//! One frame is retained until completely written. A partial write followed by
//! WouldBlock is normal; replacing its suffix with a newer datagram corrupts
//! every subsequent message. Hard IO failures are terminal, never a reason to
//! resume the same bytes on a replacement connection/allocation.
//!
//! This is a transport building block, not yet a client-selectable TCP/TLS path.

use std::io::{self, Write};

use lowlat_core::turn::stream::{MAX_FRAME_LEN, encode};

/// Limits work per writable event even if a writer accepts one byte at a time
/// or repeatedly interrupts, leaving time for receive processing and deadlines.
const WRITE_CALLS_PER_PASS: usize = 64;

/// One pending, bounded TURN frame. Allocated once, never per queued message.
#[derive(Debug)]
pub struct Output {
    storage: Box<[u8]>,
    used: usize,
    written: usize,
    failed: bool,
}

impl Output {
    /// The capacity is an explicit wire-frame limit, between a STUN header
    /// and MAX_FRAME_LEN. It does not describe a desired buffering delay.
    pub fn new(capacity: usize) -> io::Result<Self> {
        if !(20..=MAX_FRAME_LEN).contains(&capacity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TURN frame capacity",
            ));
        }
        Ok(Self {
            storage: vec![0; capacity].into_boxed_slice(),
            used: 0,
            written: 0,
            failed: false,
        })
    }

    /// Queue a complete TURN message. WouldBlock means the caller must retain
    /// its message and wait for output progress, not consume more core output.
    pub fn queue(&mut self, message: &[u8]) -> io::Result<()> {
        self.check_live()?;
        if self.pending() != 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.used = encode(message, &mut self.storage)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid TURN frame"))?;
        self.written = 0;
        Ok(())
    }

    /// Bytes not yet accepted by the writer. Acceptance is not peer delivery;
    /// a TLS writer can also retain ciphertext awaiting socket writability.
    pub fn pending(&self) -> usize {
        self.used.saturating_sub(self.written)
    }

    /// Try progress on a nonblocking writer, after writable readiness (or on
    /// enqueue). Returns true once no frame remains, false on backpressure or
    /// the fairness limit. The caller must keep writable interest while false.
    /// No blocking flush, sleep or retry-until-ready loop is used here.
    pub fn write_ready(&mut self, writer: &mut impl Write) -> io::Result<bool> {
        self.check_live()?;
        for _ in 0..WRITE_CALLS_PER_PASS {
            if self.pending() == 0 {
                self.used = 0;
                self.written = 0;
                return Ok(true);
            }
            let bytes = self.storage.get(self.written..self.used).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "Invalid TURN write offset")
            })?;
            match writer.write(bytes) {
                Ok(0) => return self.fail(io::ErrorKind::WriteZero.into()),
                Ok(count) if count <= bytes.len() => self.written += count,
                Ok(_) => return self.fail(io::ErrorKind::InvalidData.into()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) => return self.fail(error),
            }
        }
        Ok(self.pending() == 0)
    }

    fn check_live(&self) -> io::Result<()> {
        if self.failed {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TURN stream failed",
            ))
        } else {
            Ok(())
        }
    }

    fn fail(&mut self, error: io::Error) -> io::Result<bool> {
        self.failed = true;
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Debug, Default)]
    struct Writer {
        steps: VecDeque<io::Result<usize>>,
        bytes: Vec<u8>,
        calls: usize,
    }

    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            let count = self.steps.pop_front().unwrap_or(Ok(bytes.len()))?;
            let count = count.min(bytes.len());
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            panic!("the readiness path must not block in flush")
        }
    }

    const MESSAGE: &[u8] = &[0x40, 0, 0, 3, 1, 2, 3];
    const WIRE: &[u8] = &[0x40, 0, 0, 3, 1, 2, 3, 0];

    #[test]
    fn partial_writes_backpressure_and_interrupts_preserve_frame() {
        let mut output = Output::new(64).unwrap();
        output.queue(MESSAGE).unwrap();
        let mut writer = Writer {
            steps: [
                Ok(2),
                Err(io::ErrorKind::Interrupted.into()),
                Ok(1),
                Err(io::ErrorKind::WouldBlock.into()),
            ]
            .into(),
            ..Writer::default()
        };
        assert!(!output.write_ready(&mut writer).unwrap());
        assert_eq!(output.pending(), 5);
        assert_eq!(
            output.queue(MESSAGE).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(output.write_ready(&mut writer).unwrap());
        assert_eq!(writer.bytes, WIRE);
        output.queue(MESSAGE).unwrap();
        assert!(output.write_ready(&mut writer).unwrap());
        assert_eq!(writer.bytes, [WIRE, WIRE].concat());
    }

    #[test]
    fn a_failed_stream_cannot_resume_or_accept_new_messages() {
        for failure in [Ok(0), Err(io::ErrorKind::ConnectionReset.into())] {
            let mut output = Output::new(64).unwrap();
            output.queue(MESSAGE).unwrap();
            let mut writer = Writer {
                steps: [Ok(2), failure].into(),
                ..Writer::default()
            };
            assert!(output.write_ready(&mut writer).is_err());
            assert!(output.write_ready(&mut writer).is_err());
            assert!(output.queue(MESSAGE).is_err());
            assert_eq!(writer.bytes, WIRE[..2]);
        }
    }

    #[test]
    fn pathological_writer_yields_to_other_session_work() {
        let mut output = Output::new(64).unwrap();
        output.queue(MESSAGE).unwrap();
        let mut writer = Writer {
            steps: (0..100)
                .map(|_| Err(io::ErrorKind::Interrupted.into()))
                .collect(),
            ..Writer::default()
        };
        assert!(!output.write_ready(&mut writer).unwrap());
        assert_eq!(writer.calls, WRITE_CALLS_PER_PASS);
        assert!(output.write_ready(&mut writer).unwrap());
        assert_eq!(writer.bytes, WIRE);
    }

    #[test]
    fn invalid_frames_are_not_sent_and_capacity_is_bounded() {
        assert!(Output::new(19).is_err());
        assert!(Output::new(MAX_FRAME_LEN + 1).is_err());
        let mut output = Output::new(20).unwrap();
        assert!(output.queue(&[0x40, 0, 0, 30]).is_err());
        assert_eq!(output.pending(), 0);
        output.queue(MESSAGE).unwrap();
    }
}
