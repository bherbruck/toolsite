# syslog

## Schema

See migrations/. Add a numbered file for each change.

## Decisions

- Not resident. Each datagram is one insert and one publish, so a fresh
  instance per event is enough.
- A datagram that does not parse is stored whole with severity NULL (shown
  as unknown), not dropped. A log receiver that loses lines it cannot read
  hides the problem it is there to show.
- The host is the sender's host name, or the remote's IP address when the
  message has none.
- The sender's timestamp is not stored. Device clocks are often wrong;
  `received_at` is the server's clock, and sorting by it is reliable.
- RFC 5424 structured data is skipped, not stored.
- `ALLOWED_SOURCES` is checked at `connect`, the first datagram from an
  address. A refused address's datagram is dropped, and the next one asks
  again.
- The page filters the live tail on its own side with the same rules as
  `/api/logs`, so the server publishes each line once to one topic.

## Unfinished

- No CIDR ranges in `ALLOWED_SOURCES`, only single addresses.
- No search in the message text.
- No TCP syslog (RFC 6587). It would be a `tcp` socket with framing by
  octet count or by newline.
