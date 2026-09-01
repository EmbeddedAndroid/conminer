# The capture path

Capture is the lowest layer in the stack. If it lies, everything above it
inherits the lie, so its rules are worth stating exactly.

    board -> ser2net -> capture -> broker -> web console, agents
                            \
                             -> framer -> miner -> store

## The reader never waits for the miner

ser2net discards for a client that does not read. Every millisecond the capture
loop spends framing, mining or writing to the store is console it will never
see again, so the socket loop does exactly three things: read, republish to the
broker, and hand the bytes to the miner.

Mining runs on its own thread (`conminer-mine`). The queue between them is
bounded. When it fills, the chunk is dropped and counted in
`stats.dropped_bytes`, and the drop is logged with the device and the byte
count. Blocking the reader instead would lose more, silently, at ser2net.

Loss must be a counter, never a silence: a gap in a console is otherwise
indistinguishable from an idle board.

An ad-hoc capture (`conminer ingest`, tests) has no miner thread and mines
inline. Nothing is racing the socket there.

## Republish before framing

Subscribers get the raw stream exactly as the board sent it, before any
framing, and they never wait on the store. The web console and the agent tools
therefore see the same bytes at the same time.

## Connection policy

- A console whose device node is absent is not dialled at all. Dialling an
  absent device makes ser2net attempt an open, fail, and log it, and the
  supervisor restarts ser2net to clear what looks like a wedge, which drops
  every console on the host.
- Only a device that has been seen present can be treated as absent. A relayed
  peer console and a file-backed device name a path that never exists locally
  while ser2net serves them perfectly well.
- The backoff ceiling is short. ser2net only holds the tty open while a client
  is connected, so a long backoff is not patience: it is buffered console that
  arrives later in one lump.
- A connection is not a console. The backoff resets only when a session carried
  real console bytes, not merely because TCP connected: ser2net accepts a
  client for a device it cannot open, serves "Device open failure", and closes.

## Capture states

| State | Meaning |
| --- | --- |
| `not_listening` | Not attached. Capture is not recording |
| `listening` | Attached, nothing arriving |
| `streaming` | Bytes arriving |
| `garbage` | Arriving bytes fail the framer's burst threshold, usually a baud mismatch |
| `open_failed` | ser2net served its device-open failure instead of console data |
| `away_in_edl` | A recovery gadget is on this board's own USB ports |

`away_in_edl` suppresses the state, never the recording. During a flash the tty
is often present and streaming the flasher's own output; those bytes are stored,
and only the state every reader sees is suppressed, because there is no OS
console to command.

Recovery gadgets are configured, not hard-coded: `capture.recovery_gadgets`
takes `vid:pid` or `vid:*` entries, so a flashing tool other than QDL is a
config change rather than a code change.

## Failure states expire

`open_failed` and `away_in_edl` are both reached by reading something and then,
by their nature, receiving nothing more, so no byte ever arrives to correct
them. Capture re-dials periodically while in one of those states, and
immediately when an absent device node returns.
