use std::io::{self, BufRead};

/// Reads a bounded line; lines exceeding `limit` are drained and reported as `TooLong`, never truncated.
pub fn read_line_bounded<R: BufRead>(reader: &mut R, limit: usize) -> io::Result<Option<LineRead>> {
    let mut line = Vec::new();
    let mut too_long = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            if line.is_empty() && !too_long {
                return Ok(None);
            }
            return Ok(Some(if too_long {
                LineRead::TooLong
            } else {
                LineRead::Line(line)
            }));
        }
        if let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            if !too_long {
                let take = newline.min(limit.saturating_sub(line.len()));
                line.extend_from_slice(&buffer[..take]);
                if take < newline {
                    too_long = true;
                }
            }
            reader.consume(newline + 1);
            return Ok(Some(if too_long {
                LineRead::TooLong
            } else {
                LineRead::Line(line)
            }));
        }
        let length = buffer.len();
        if !too_long {
            let take = length.min(limit.saturating_sub(line.len()));
            line.extend_from_slice(&buffer[..take]);
            if take < length {
                too_long = true;
            }
        }
        reader.consume(length);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum LineRead {
    Line(Vec<u8>),
    TooLong,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn reads_lines_and_handles_crlf() {
        let mut reader = Cursor::new(b"one\r\ntwo\n".to_vec());
        assert_eq!(
            read_line_bounded(&mut reader, 10).unwrap(),
            Some(LineRead::Line(b"one\r".to_vec()))
        );
        assert_eq!(
            read_line_bounded(&mut reader, 10).unwrap(),
            Some(LineRead::Line(b"two".to_vec()))
        );
        assert_eq!(read_line_bounded(&mut reader, 10).unwrap(), None);
    }

    #[test]
    fn oversized_line_is_drained_once() {
        let mut reader = Cursor::new(b"0123456789\nnext\n".to_vec());
        assert_eq!(
            read_line_bounded(&mut reader, 4).unwrap(),
            Some(LineRead::TooLong)
        );
        assert_eq!(
            read_line_bounded(&mut reader, 4).unwrap(),
            Some(LineRead::Line(b"next".to_vec()))
        );
        assert_eq!(read_line_bounded(&mut reader, 4).unwrap(), None);
    }

    #[test]
    fn final_line_without_newline_is_returned() {
        let mut reader = Cursor::new(b"tail".to_vec());
        assert_eq!(
            read_line_bounded(&mut reader, 10).unwrap(),
            Some(LineRead::Line(b"tail".to_vec()))
        );
        assert_eq!(read_line_bounded(&mut reader, 10).unwrap(), None);
    }
}
