//! Incremental streaming JSON response extractor for the Steer Arbitrator.

#[derive(Default)]
pub struct StreamingResponseExtractor {
    buffer: String,
    pub in_response_field: bool,
    pub finished: bool,
    escape_next: bool,
    unicode_buffer: Option<String>,
}

impl StreamingResponseExtractor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_chunk(&mut self, chunk: &str) -> (String, bool) {
        if self.finished {
            return (String::new(), false);
        }

        let mut output = String::new();
        let mut just_finished = false;

        if !self.in_response_field {
            self.buffer.push_str(chunk);
            // Look for "response" : "
            if let Some(pos) = self.buffer.find("\"response\"") {
                let rest = &self.buffer[pos + "\"response\"".len()..];
                if let Some(colon_pos) = rest.find(':') {
                    let after_colon = &rest[colon_pos + 1..];
                    let trimmed = after_colon.trim_start();
                    if trimmed.starts_with('"') {
                        let quote_pos = after_colon.find('"').unwrap();
                        self.in_response_field = true;
                        let content_after_quote = &after_colon[quote_pos + 1..];
                        let chars: Vec<char> = content_after_quote.chars().collect();
                        self.buffer.clear();
                        for ch in chars {
                            if self.process_char(ch, &mut output) {
                                just_finished = true;
                                break;
                            }
                        }
                        return (output, just_finished);
                    } else if trimmed.starts_with("null")
                        || (!trimmed.is_empty()
                            && !trimmed.starts_with('"')
                            && !trimmed.starts_with('n'))
                    {
                        self.finished = true;
                        self.buffer.clear();
                        return (String::new(), false);
                    }
                }
            }
            return (String::new(), false);
        }

        // Already in response field
        for ch in chunk.chars() {
            if self.process_char(ch, &mut output) {
                just_finished = true;
                break;
            }
        }

        (output, just_finished)
    }

    fn process_char(&mut self, ch: char, output: &mut String) -> bool {
        if let Some(ref mut ubuf) = self.unicode_buffer {
            ubuf.push(ch);
            if ubuf.len() == 4 {
                if let Ok(code) = u32::from_str_radix(ubuf, 16)
                    && let Some(unicode_char) = char::from_u32(code)
                {
                    output.push(unicode_char);
                }
                self.unicode_buffer = None;
            }
            return false;
        }

        if self.escape_next {
            self.escape_next = false;
            match ch {
                'n' => output.push('\n'),
                'r' => output.push('\r'),
                't' => output.push('\t'),
                '\\' => output.push('\\'),
                '"' => output.push('"'),
                '/' => output.push('/'),
                'u' => self.unicode_buffer = Some(String::new()),
                other => {
                    output.push('\\');
                    output.push(other);
                }
            }
            return false;
        }

        if ch == '\\' {
            self.escape_next = true;
            return false;
        }

        if ch == '"' {
            self.finished = true;
            self.in_response_field = false;
            return true;
        }

        output.push(ch);
        false
    }
}
