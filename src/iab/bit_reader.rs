use super::IabError;

pub(super) struct BitReader<'a> {
    bytes: &'a [u8],
    bit_position: usize,
    element: &'static str,
}

impl<'a> BitReader<'a> {
    pub(super) fn new(bytes: &'a [u8], element: &'static str) -> Self {
        Self {
            bytes,
            bit_position: 0,
            element,
        }
    }

    fn truncated(&self, field: &'static str) -> IabError {
        IabError::Truncated {
            element: self.element,
            field,
        }
    }

    pub(super) fn invalid(&self, field: &'static str, value: u64, rule: &'static str) -> IabError {
        IabError::Invalid {
            element: self.element,
            field,
            value,
            rule,
        }
    }

    pub(super) fn read(&mut self, bit_count: u32, field: &'static str) -> Result<u32, IabError> {
        debug_assert!(bit_count <= 32);
        let end = self.bit_position + bit_count as usize;
        if end > self.bytes.len() * 8 {
            return Err(self.truncated(field));
        }
        let mut value: u64 = 0;
        let mut position = self.bit_position;
        while position < end {
            let bit_in_byte = position % 8;
            let taken = (8 - bit_in_byte).min(end - position);
            let byte = u64::from(self.bytes[position / 8]);
            let bits = (byte >> (8 - bit_in_byte - taken)) & ((1 << taken) - 1);
            value = (value << taken) | bits;
            position += taken;
        }
        self.bit_position = end;
        Ok(value as u32)
    }

    pub(super) fn read_flag(&mut self, field: &'static str) -> Result<bool, IabError> {
        Ok(self.read(1, field)? == 1)
    }

    // ST 2098-2 5.2: an all-ones field escapes to one twice as wide, up to 32 bits
    pub(super) fn read_plex(
        &mut self,
        first_width: u32,
        field: &'static str,
    ) -> Result<u32, IabError> {
        let mut width = first_width;
        loop {
            let value = self.read(width, field)?;
            let escape = if width == 32 {
                u32::MAX
            } else {
                (1 << width) - 1
            };
            if value != escape {
                return Ok(value);
            }
            if width == 32 {
                return Err(self.invalid(
                    field,
                    u64::from(value),
                    "a Plex value is at most 0xFFFFFFFE",
                ));
            }
            width *= 2;
        }
    }

    pub(super) fn align(&mut self) {
        self.bit_position = self.bit_position.div_ceil(8) * 8;
    }

    pub(super) fn expect_end(&mut self, size_field: &'static str) -> Result<(), IabError> {
        self.align();
        let unread_bytes = self.remaining_bytes();
        if unread_bytes != 0 {
            return Err(self.invalid(
                size_field,
                unread_bytes as u64,
                "the element's fields fill its size",
            ));
        }
        Ok(())
    }

    pub(super) fn remaining_bytes(&self) -> usize {
        self.bytes.len() - self.bit_position.div_ceil(8)
    }

    // callers align first, every byte run in the syntax starts on a byte boundary
    pub(super) fn read_bytes(
        &mut self,
        count: usize,
        field: &'static str,
    ) -> Result<&'a [u8], IabError> {
        debug_assert!(self.bit_position.is_multiple_of(8));
        let start = self.bit_position / 8;
        let end = start
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| self.truncated(field))?;
        self.bit_position = end * 8;
        Ok(&self.bytes[start..end])
    }

    pub(super) fn read_rest(&mut self) -> &'a [u8] {
        self.align();
        let start = self.bit_position / 8;
        self.bit_position = self.bytes.len() * 8;
        &self.bytes[start..]
    }

    pub(super) fn read_null_terminated(
        &mut self,
        field: &'static str,
    ) -> Result<Vec<u8>, IabError> {
        self.align();
        let start = self.bit_position / 8;
        let length = self.bytes[start..]
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| self.truncated(field))?;
        self.bit_position = (start + length + 1) * 8;
        Ok(self.bytes[start..start + length].to_vec())
    }
}
