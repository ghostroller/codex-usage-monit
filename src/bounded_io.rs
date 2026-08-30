use std::io::{self, BufRead};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoundedLine {
    Line,
    TooLong,
    Eof,
}

/// Reads one newline-delimited byte record without ever growing `output`
/// beyond `max_bytes`. Oversized records are drained through their newline so
/// the caller can safely continue with the next record.
pub(crate) fn read_bounded_line(
    reader: &mut impl BufRead,
    output: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<BoundedLine> {
    output.clear();
    let mut saw_bytes = false;
    let mut too_long = false;
    // Delay committing CR until the following byte is known. This keeps the
    // CR in a CRLF terminator out of the payload limit even when the pair is
    // split across two `fill_buf` calls, while preserving an ordinary or
    // unterminated CR as content.
    let mut pending_cr = false;

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if pending_cr && !too_long {
                if output.len() < max_bytes {
                    output.push(b'\r');
                } else {
                    output.clear();
                    too_long = true;
                }
            }
            return Ok(if !saw_bytes {
                BoundedLine::Eof
            } else if too_long {
                BoundedLine::TooLong
            } else {
                BoundedLine::Line
            });
        }

        let mut consumed = 0_usize;
        let mut ended = false;
        for &byte in available {
            consumed += 1;
            saw_bytes = true;

            if pending_cr {
                if byte == b'\n' {
                    pending_cr = false;
                    ended = true;
                    break;
                }
                if !too_long {
                    if output.len() < max_bytes {
                        output.push(b'\r');
                    } else {
                        output.clear();
                        too_long = true;
                    }
                }
                pending_cr = false;
            }

            match byte {
                b'\n' => {
                    ended = true;
                    break;
                }
                b'\r' => pending_cr = true,
                _ if !too_long => {
                    if output.len() < max_bytes {
                        output.push(byte);
                    } else {
                        output.clear();
                        too_long = true;
                    }
                }
                _ => {}
            }
        }

        reader.consume(consumed);
        if ended {
            return Ok(if too_long {
                BoundedLine::TooLong
            } else {
                BoundedLine::Line
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, BufRead, BufReader, Cursor, Read};

    use super::{BoundedLine, read_bounded_line};

    #[test]
    fn accepts_the_exact_limit_and_an_unterminated_last_record() {
        let mut reader = BufReader::new(Cursor::new(b"1234\nlast"));
        let mut output = Vec::new();

        assert_eq!(
            read_bounded_line(&mut reader, &mut output, 4).unwrap(),
            BoundedLine::Line
        );
        assert_eq!(output, b"1234");
        assert_eq!(
            read_bounded_line(&mut reader, &mut output, 4).unwrap(),
            BoundedLine::Line
        );
        assert_eq!(output, b"last");
        assert_eq!(
            read_bounded_line(&mut reader, &mut output, 4).unwrap(),
            BoundedLine::Eof
        );
    }

    #[test]
    fn drains_an_oversized_record_and_recovers_at_the_next_newline() {
        let mut reader = BufReader::with_capacity(3, Cursor::new(b"123456789\nok\n"));
        let mut output = Vec::new();

        assert_eq!(
            read_bounded_line(&mut reader, &mut output, 4).unwrap(),
            BoundedLine::TooLong
        );
        assert!(output.is_empty());
        assert_eq!(
            read_bounded_line(&mut reader, &mut output, 4).unwrap(),
            BoundedLine::Line
        );
        assert_eq!(output, b"ok");
    }

    #[test]
    fn crlf_does_not_consume_payload_capacity_across_buffer_boundaries() {
        let mut reader = BufReader::with_capacity(1, Cursor::new(b"1234\r\nok\r\nlast\r"));
        let mut output = Vec::new();

        assert_eq!(
            read_bounded_line(&mut reader, &mut output, 4).unwrap(),
            BoundedLine::Line
        );
        assert_eq!(output, b"1234");
        assert_eq!(
            read_bounded_line(&mut reader, &mut output, 4).unwrap(),
            BoundedLine::Line
        );
        assert_eq!(output, b"ok");
        assert_eq!(
            read_bounded_line(&mut reader, &mut output, 4).unwrap(),
            BoundedLine::TooLong
        );
        assert!(output.is_empty());
    }

    struct ErrorAfterCr {
        returned_cr: bool,
    }

    impl Read for ErrorAfterCr {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            unreachable!("BufRead implementation is used directly")
        }
    }

    impl BufRead for ErrorAfterCr {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            if self.returned_cr {
                Err(io::Error::other("injected read failure"))
            } else {
                Ok(b"\r")
            }
        }

        fn consume(&mut self, amount: usize) {
            assert_eq!(amount, 1);
            self.returned_cr = true;
        }
    }

    #[test]
    fn propagates_an_error_after_a_pending_cr() {
        let mut reader = ErrorAfterCr { returned_cr: false };
        let mut output = Vec::new();

        let error = read_bounded_line(&mut reader, &mut output, 4).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "injected read failure");
        assert!(output.is_empty());
    }
}
