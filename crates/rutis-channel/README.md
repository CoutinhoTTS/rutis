# rutis-channel

Channels that carry rutis protocols without knowing them: ordered, reliable,
message-bounded duplex links, split into a sender, a receiver and a closer.
`rutis-interop` runs its sessions on them.

- `Channel::unix(stream)`: a Unix stream socket, one message per line.
- `rutis_channel::pair(capacity)`: two connected in-process channels.
- `Channel::with_end_reason(f)`: report the end of a channel as `f()`, for
  example the exit status of the process at the other end.

The interfaces block on purpose: a session may wait for a reply on any
thread, including the only thread of a current-thread runtime, so a channel
must make progress without the caller's executor.

Design: [protocol and channel decoupling](../../docs/design-protocol-channel-decoupling-2026-10-03.md).
