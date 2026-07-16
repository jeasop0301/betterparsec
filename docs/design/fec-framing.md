# FEC transport framing

## Compatibility and selection

`video_fec` and `video_fec_ack` carry little-endian binary messages. Version 1
remains supported for degraded compatibility: source kind `0x00`, repair kind
`0x01`, one-byte Subscribe `0x01`, one-byte NeedsIdr `0x00`, and four-byte LE
ACK are unchanged. A receiver selects v2 only from `0x02`/`0x03` symbol kinds
or `0x80`–`0x82` control kinds. A selected v2 message which is malformed is
rejected; it is never retried as v1.

An epoch is sender-owned, nonzero, and changes at a stream discontinuity.
Sequence comparisons use RFC 1982 half-range arithmetic.

## v2 symbol messages (`video_fec`)

All fields below are little-endian and CRCs are standard IEEE CRC-32.

```
source:
  [0x02][epoch:u32][seq:u32][payload_len:u16][payload_crc32:u32][chunk_v2]

repair:
  [0x03][epoch:u32][repair_seq:u16][window_base:u32][window_end:u32]
  [payload_len:u16][payload_crc32:u32][payload]
```

The source header is 15 bytes. Source messages are at most 1200 bytes, so a
source payload is at most 1185 bytes. The repair header is 21 bytes; repair
payloads are at most 1200 bytes and repair messages are at most 1221 bytes.
The parser validates the declared length, cap, and CRC before it allocates a
payload.

## v2 chunk payload

`chunk_v2` is a 21-byte header followed by a fragment:

```
[frame_id:u32][chunk_index:u16][chunk_count:u16][frame_type:u8]
[timestamp_us:u32][encoded_frame_len:u32][encoded_frame_crc32:u32][fragment]
```

`frame_type` is `0` for delta and `1` for key. `encoded_frame_len` and
`encoded_frame_crc32` describe the complete encoded frame, not an individual
fragment. Fragment length is at most 1164 bytes (`1185 - 21`). A frame is at
most 4 MiB and has 1 through 4096 chunks; `chunk_index < chunk_count`.
Receivers validate header bounds before retaining fragments, then validate the
complete frame length and IEEE CRC-32 after reassembly.

## v2 controls (`video_fec_ack`)

```
Subscribe: [0x82][epoch:u32]
ACK:       [0x81][epoch:u32][highest_seq:u32]
NeedsIdr:  [0x80][epoch:u32][reason:u8]
```

Controls are exact-length messages. The epoch makes acknowledgements and IDR
requests from a prior stream instance unambiguous.

## Resource limits

Reassembly plus completed-frame reordering retains at most 8 frame records and
16 MiB of fragments. FEC algebra accepts at most 128 symbols and 16 MiB of
data. These bounds, along with the wire caps above, are enforced before
untrusted input causes allocation. A chunk for an already completed frame, or
a duplicate fragment of a pending frame, is discarded before it can consume a
record or byte-cap budget.

Completed frames are reordered only to fill a missing predecessor. The receiver
waits while at most `REORDER_MAX_COMPLETED_FRAMES` (4) completed frames are
queued and the oldest has waited less than `REORDER_MAX_WAIT_MS` (100 ms).
At the fifth queued completed frame or once the oldest reaches 100 ms, a
missing predecessor is a reorder discontinuity: queued state is discarded and
an IDR is requested.
