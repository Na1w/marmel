    /// F7: click within the input area places the cursor at the
    /// nearest grapheme boundary.
    fn click_to_cursor(&mut self, x: u16, area: Rect) {
        let relative_x = x.saturating_sub(area.x as u16);
        let mut byte_offset = 0;
        let mut visual_x = 0;
        for grapheme in self.input_text.graphemes(true) {
            if visual_x > relative_x {
                break;
            }
            byte_offset += grapheme.len();
            visual_x += UnicodeWidthStr::width(grapheme) as u16;
        }
        self.cursor = byte_offset;
    }
