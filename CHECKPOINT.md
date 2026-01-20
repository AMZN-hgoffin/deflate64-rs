# Checkpoints (experimental)

## Overview

`checkpoint()` and `restore_from_checkpoint()` allow partial decompression progress to be saved and restored across different processes. This can be used to restore partial progress after being interrupted when decompressing very long streams or when read/write storage access is very slow.

## Stability Warning

The checkpoint serialization format is experimental and not guaranteed to be stable across library versions. Checkpoint data includes an internal version number, and `restore_from_checkpoint()` will return `None` if the checkpoint was created by an incompatible version. Do not rely on checkpoints persisting across library upgrades.

## API

```rust
pub fn checkpoint(&self) -> Option<(Vec<u8>, CheckpointStreamPositions)>

pub fn restore_from_checkpoint(&mut self, checkpoint_data: &[u8]) -> Option<CheckpointStreamPositions>

pub struct CheckpointStreamPositions {
    pub input_bytes_to_skip: u64,            // caller must seek input to this byte offset
    pub output_bytes_already_returned: u64,  // caller must skip this many output bytes
}
```

## Storing progress with a checkpoint

```rust
    const CHECKPOINT_INTERVAL: u64 = 100_000_000; // every 100 MB
    let mut input_buf = [0u8; 8192];
    let mut output_buf = [0u8; 8192];
    let mut bytes_written = 0u64;
    let mut checkpoint_counter = 0u64;
    loop {
        let n = input.read(&mut input_buf)?;
        if n == 0 && inflater.finished() {
            break;
        }

        let result = inflater.inflate(&input_buf[..n], &mut output_buf);
        output.write_all(&output_buf[..result.bytes_written])?;
        bytes_written += result.bytes_written as u64;

        if bytes_written / CHECKPOINT_INTERVAL != checkpoint_counter {
            checkpoint_counter = bytes_written / CHECKPOINT_INTERVAL;
            if let Some((data, _positions)) = inflater.checkpoint() {
                std::fs::write("checkpoint.dat", &data)?;
            }
        }
    }
```

## Restoring progress from a checkpoint

To resume from a checkpoint:
1. Call `restore_from_checkpoint()`, which returns `None` if the checkpoint data is corrupt, invalid, or from an incompatible version
2. Seek the input stream to `input_bytes_to_skip` as counted from the start of the stream
3. Seek the output stream to `output_bytes_already_returned` as counted from the start of the stream
4. Resume decompression with a traditional `inflate()` processing loop

```rust
    let mut inflater = InflaterManaged::new();
    if let Some(positions) = inflater.restore_from_checkpoint(checkpoint_data) {
        input.seek(SeekFrom::Start(positions.input_bytes_to_skip))?;
        output.seek(SeekFrom::Start(positions.output_bytes_already_returned))?;
    } else {
        // Checkpoint restoration failed, process streams from the beginning
    }

    // Proceed with standard inflate() loop
```

## Security Note

Checkpoint data represents internal program state, and invalid or corrupt checkpoint data cannot always be detected. Do not restore checkpoints from untrusted sources as this may lead to decompression errors or incorrect `inflate()` output.

## Validation

The `restore_from_checkpoint()` function performs the following validation:
- Fletcher-32 checksum verification
- Window data length must match expected size based on output position and unread bytes
- Code lengths for dynamic blocks must be in range 0-16
- Huffman trees for dynamic blocks must be valid
- Uncompressed blocks with remaining bytes must be byte-aligned (bits_in_buffer == 0)
- If inflater was created with `with_uncompressed_size()`, checkpoint must not exceed that limit

## Internal Details

When the "checkpoint" feature is enabled, the inflater keeps some additional internal variables:
- `checkpoint_input_bits`: exact input bit position at time of checkpoint
- `checkpoint_bit_buffer`: low byte of bit_buffer at time of checkpoint
- `checkpoint_bfinal_block_type`: combined bfinal flag (high bit) and block_type (low bits)

These are updated after every internal write to the output window buffer, when the decoder is in the DecodeTop or DecodingUncompressed internal states, or when transitioning between deflate blocks. The internal checkpoint variables and related state, including the history window, are serialized into a byte buffer when `checkpoint()` is called. The process is reversed by `restore_from_checkpoint()` which reconstructs internal state from the byte buffer, including the preserved contents of the output window.

## Serialization Format

The size of the byte buffer returned from `checkpoint()` will generally be 65KB, although it can be as large as 131KB if the inflater contains the maximum possible amount of buffered output which has not yet been drained by the caller.

The format is a fixed 346-byte header followed by variable-length window data and a trailing checksum:

```
Offset  Size  Field (always little-endian)
------  ----  ----------------------------------
0       2     checkpoint_version: u16       # version field - currently 0x1001
2       8     input_bits: u64               # exact input bit position
10      1     buffered_value: u8            # low byte of input bit buffer (contains 0-7 unread bits)
11      1     bfinal_block_type: u8         # (bfinal << 7) | block_type
12      2     uncompressed_remaining: u16   # bytes left in uncompressed block (0 if not in uncompressed block)
14      288   lit_code_lengths: [u8; 288]   # dynamic block: code lengths (0-16), zero-padded; else all zero
302     32    dist_code_lengths: [u8; 32]   # dynamic block: code lengths (0-16), zero-padded; else all zero
334     8     output_bytes_written: u64     # total bytes ever written to window
342     4     output_bytes_unread: u32      # bytes in window not yet returned to caller
346     var   window_data: [u8]             # len = max(min(65538, output_bytes_written), output_bytes_unread)
END-4   4     checksum: u32                 # Fletcher-32 checksum of preceding bytes
```

The serialized window_data contains all "reachable" bytes from the output window. At a minimum, the includes the most recent 65538 bytes which can be referenced by DEFLATE64 distance codes. The output window also buffers output which has not yet been returned to the caller, and so if the caller is not draining output bytes fast enough, the checkpoint must include all unread bytes (up to 128KB, the window size).
