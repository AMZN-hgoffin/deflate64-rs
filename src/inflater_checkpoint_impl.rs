// Checkpoint serialization for InflaterManaged. This file is included into
// inflater_managed.rs only when the "checkpoint" feature is enabled.
//
// The update_checkpoint_after_write_or_eob() helper updates checkpoint state
// after every output window write when in DecodeTop or DecodingUncompressed
// states, or when in between deflate blocks. See CHECKPOINT.md for details.
//
// Checkpoints are serialized by the checkpoint() function as follows:
//
//   Offset  Size  Field
//   0       2     version (0x1001)
//   2       8     input_bits
//   10      1     buffered_value (masked on restore by from_bits)
//   11      1     bfinal_block_type
//   12      2     uncompressed_remaining
//   14      288   lit_code_lengths
//   302     32    dist_code_lengths
//   334     8     output_bytes_written
//   342     4     output_bytes_unread
//   346     var   window_data
//   end-4   4     fletcher32 checksum

const CHECKPOINT_HEADER_SIZE: usize = 346;

fn fletcher32_checksum(data: &[u8]) -> u32 {
    let (mut a, mut b) = (0u32, 0u32);
    for &byte in data {
        a = a.wrapping_add(byte as u32);
        b = b.wrapping_add(a);
    }
    (b << 16) | (a & 0xFFFF)
}

impl InflaterManaged {
    #[inline(always)]
    fn update_checkpoint_after_write_or_eob(&mut self, input: &InputBuffer<'_>, end_of_block: bool) {
        debug_assert!(input.available_bits() >= 0 && input.available_bits() <= 32);
        // checkpoint_input_bits tracks the number of input bits which have been consumed up to
        // the moment of the checkpoint. It does not count unconsumed bits still in the buffer.
        self.checkpoint_input_bits = (self.total_input_loaded + input.read_bytes as u64) * 8
            - input.available_bits() as u64;
        // checkpoint_bit_buffer holds the unconsumed bits of the most recently loaded input byte.
        // When the input bit position is byte-aligned, the value of checkpoint_bit_buffer is 0.
        self.checkpoint_bit_buffer = input.peek_available_bits() as u8;
        // checkpoint_bfinal_block_type holds the bfinal state and the current block type at the
        // moment of the checpoint. The end-of-block state is stored as an uncompressed block
        // with zero bytes remaining, as this is functionally identical to an end-of-block state. 
        let bfinal_flag = (self.bfinal as u8) << 7;
        if end_of_block {
            debug_assert!(matches!(self.state, InflaterState::ReadingBFinal | InflaterState::Done));
            self.checkpoint_bfinal_block_type = BlockType::Uncompressed as u8 | bfinal_flag;
        } else {
            match self.block_type {
                BlockType::Uncompressed => {
                    debug_assert_eq!(self.state, InflaterState::DecodingUncompressed);
                    debug_assert!(self.block_length > 0);
                },
                BlockType::Static => debug_assert_eq!(self.state, InflaterState::DecodeTop),
                BlockType::Dynamic => debug_assert_eq!(self.state, InflaterState::DecodeTop),
            };
            self.checkpoint_bfinal_block_type = self.block_type as u8 | bfinal_flag;
        }
    }

    /// Serialize the most recent inflater checkpoint for use with restore_from_checkpoint().
    /// A serialized checkpoint can contain up to 129KB of data representing the state of the
    /// inflater and its internal history buffer. Not all input or output byte offsets can
    /// be represented by a checkpoint; the CheckpointStreamPositions structure describes the
    /// input and output byte offsets which correspond to the checkpoint being serialized.
    #[cfg_attr(docsrs, doc(cfg(feature = "checkpoint")))]
    pub fn checkpoint(&self) -> Option<(Vec<u8>, CheckpointStreamPositions)> {
        if self.checkpoint_input_bits == 0
            || self.errored()
            || (self.output.available_bytes() == 0 && self.state == InflaterState::Done)
        {
            return None;
        }

        let checkpoint_block_type =
            BlockType::from_int((self.checkpoint_bfinal_block_type & 0x7F) as u16)?;
        let uncompressed_remaining = match checkpoint_block_type {
            BlockType::Uncompressed => self.block_length as u32,
            _ => 0,
        };

        let mut lit_codes = [0; HuffmanTree::MAX_LITERAL_TREE_ELEMENTS];
        let mut dist_codes = [0; HuffmanTree::MAX_DIST_TREE_ELEMENTS];
        if checkpoint_block_type == BlockType::Dynamic {
            let lens = self.literal_length_tree.code_lengths();
            lit_codes[..lens.len()].copy_from_slice(lens);
            let lens = self.distance_tree.code_lengths();
            dist_codes[..lens.len()].copy_from_slice(lens);
        }

        // window data slices may be split due to circular buffer
        let output_bytes_written =
            self.total_output_consumed + self.output.available_bytes() as u64;
        let bytes_unread = self.output.available_bytes() as u32;
        let (window_a, window_b) = self.output.get_checkpoint_data(output_bytes_written);

        let bfinal_block_type = self.checkpoint_bfinal_block_type;

        // Mask unrefereenced high bits in buffered byte for deterministic serialization
        let num_buffered_bits = (8 - (self.checkpoint_input_bits & 7)) as u32 & 7;
        let buffered_value = self.checkpoint_bit_buffer & ((1 << num_buffered_bits) - 1);

        let mut out = Vec::with_capacity(CHECKPOINT_HEADER_SIZE + window_a.len() + window_b.len());
        out.extend_from_slice(&0x1001u16.to_le_bytes()); // 2 - version
        out.extend_from_slice(&self.checkpoint_input_bits.to_le_bytes()); // 8
        out.push(buffered_value); // 1
        out.push(bfinal_block_type); // 1
        out.extend_from_slice(&(uncompressed_remaining as u16).to_le_bytes()); // 2
        out.extend_from_slice(&lit_codes); // 288
        out.extend_from_slice(&dist_codes); // 32
        out.extend_from_slice(&output_bytes_written.to_le_bytes()); // 8
        out.extend_from_slice(&bytes_unread.to_le_bytes()); // 4
        debug_assert_eq!(out.len(), CHECKPOINT_HEADER_SIZE);
        out.extend_from_slice(window_a);
        out.extend_from_slice(window_b);
        let checksum = fletcher32_checksum(&out);
        out.extend_from_slice(&checksum.to_le_bytes());

        let positions = CheckpointStreamPositions {
            // round up; partial input byte is already stored in checkpoint
            input_bytes_to_skip: self.checkpoint_input_bits.div_ceil(8),
            output_bytes_already_returned: output_bytes_written - bytes_unread as u64,
        };
        Some((out, positions))
    }

    /// Restore inflater state from a previous serialized checkpoint. Returns None if data
    /// is corrupt or the checkpoint was serialized by an incompatible library version. On
    /// success, the inflater's internal state is overwritten and the caller must adjust the
    /// input/output of the next inflate() operation according to the returned offsets. The
    /// offsets will be the same as those originally returned by the checkpoint() function.
    /// Note, if the inflater has a total output byte limit due to with_uncompressed_size(),
    /// the output byte limit will be retained after restoration and adjusted to account for
    /// output bytes already written before the checkpoint. Any checkpoint which has already
    /// exceeded the current output byte limit will not be restored and instead return None.
    #[cfg_attr(docsrs, doc(cfg(feature = "checkpoint")))]
    #[must_use]
    pub fn restore_from_checkpoint(
        &mut self,
        checkpoint_data: &[u8],
    ) -> Option<CheckpointStreamPositions> {
        if checkpoint_data.len() < CHECKPOINT_HEADER_SIZE + 4 {
            return None;
        }
        let (data, checksum_bytes) = checkpoint_data.split_at(checkpoint_data.len() - 4);
        let stored_checksum = u32::from_le_bytes(checksum_bytes.try_into().ok()?);
        if fletcher32_checksum(data) != stored_checksum {
            return None;
        }
        let mut cursor = data;
        let mut read = |n: usize| -> Option<&[u8]> {
            if cursor.len() < n {
                return None;
            }
            let (head, tail) = cursor.split_at(n);
            cursor = tail;
            Some(head)
        };

        // Parse all fields
        let version: u16 = u16::from_le_bytes(read(2)?.try_into().ok()?);
        if version != 0x1001 {
            return None; // unsupported version
        }
        let input_bits: u64 = u64::from_le_bytes(read(8)?.try_into().ok()?);
        let buffered_value: u8 = read(1)?[0];
        let bfinal_block_type: u8 = read(1)?[0];
        let remaining_uncompressed: u16 = u16::from_le_bytes(read(2)?.try_into().ok()?);
        let lit_codes: &[u8] = read(HuffmanTree::MAX_LITERAL_TREE_ELEMENTS)?;
        let dist_codes: &[u8] = read(HuffmanTree::MAX_DIST_TREE_ELEMENTS)?;
        let output_bytes_written: u64 = u64::from_le_bytes(read(8)?.try_into().ok()?);
        let output_bytes_unread: u32 = u32::from_le_bytes(read(4)?.try_into().ok()?);
        let window_data: &[u8] = cursor; // remaining bytes

        // from_bits masks off invalid high bits
        let num_buffered_bits = (8 - (input_bits & 7)) as i32 & 7;
        let bits = BitsBuffer::from_bits(buffered_value as u32, num_buffered_bits);

        // Check window data length validity
        let expected_window_len = (output_bytes_written.min(TABLE_LOOKUP_DISTANCE_MAX as u64)
            as u32)
            .max(output_bytes_unread) as usize;
        if window_data.len() != expected_window_len
            || window_data.len() > crate::output_window::WINDOW_SIZE
        {
            return None;
        }

        // Check uncompressed_size limit if set
        let output_already_returned = output_bytes_written - output_bytes_unread as u64;
        if self.uncompressed_size != usize::MAX
            && output_already_returned > self.uncompressed_size as u64
        {
            return None;
        }

        let bfinal = (bfinal_block_type & 128) != 0;
        let block_type_val = bfinal_block_type % 128;
        let block_type = BlockType::from_int(block_type_val.into())?;

        let mut lit_tree = HuffmanTree::invalid();
        let mut dist_tree = HuffmanTree::invalid();
        if block_type == BlockType::Dynamic {
            if lit_codes.iter().any(|x| *x > 16) || dist_codes.iter().any(|x| *x > 16) {
                return None;
            }
            lit_tree.new_in_place(lit_codes).ok()?;
            dist_tree.new_in_place(dist_codes).ok()?;
        } else if block_type == BlockType::Uncompressed && remaining_uncompressed > 0 {
            // Uncompressed blocks with remaining bytes must be byte-aligned
            if bits.bits_in_buffer != 0 {
                return None;
            }
        }

        // All validation passed - modify self
        // Pre-load buffered bits into bit buffer
        self.bits = bits;
        self.checkpoint_input_bits = input_bits;
        self.checkpoint_bit_buffer = buffered_value;
        self.total_output_consumed = output_bytes_written - output_bytes_unread as u64;
        self.current_inflated_count = self.total_output_consumed as usize;
        self.total_input_loaded = input_bits.div_ceil(8); // caller will provide input starting at input_bytes_to_skip

        self.output
            .restore_from_checkpoint(window_data, output_bytes_unread as usize);

        self.checkpoint_bfinal_block_type = bfinal_block_type;
        match block_type {
            BlockType::Uncompressed => {
                self.bfinal = bfinal;
                self.block_type = BlockType::Uncompressed;
                self.block_length = remaining_uncompressed as usize;
                if remaining_uncompressed > 0 {
                    self.state = InflaterState::DecodingUncompressed;
                } else if !bfinal {
                    self.state = InflaterState::ReadingBFinal;
                } else {
                    self.state = InflaterState::Done;
                }
            }
            BlockType::Static => {
                self.bfinal = bfinal;
                self.block_type = BlockType::Static;
                self.literal_length_tree = HuffmanTree::static_literal_length_tree();
                self.distance_tree = HuffmanTree::static_distance_tree();
                self.state = InflaterState::DecodeTop;
            }
            BlockType::Dynamic => {
                self.bfinal = bfinal;
                self.block_type = BlockType::Dynamic;
                self.literal_length_tree = lit_tree;
                self.distance_tree = dist_tree;
                self.state = InflaterState::DecodeTop;
            }
        }

        Some(CheckpointStreamPositions {
            // round up; partial input byte is already stored in checkpoint
            input_bytes_to_skip: input_bits.div_ceil(8),
            output_bytes_already_returned: output_bytes_written - output_bytes_unread as u64,
        })
    }
}
