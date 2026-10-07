# tcp-chat

## Schema

See migrations/. Add a numbered file for each change.

## Decisions

- Not resident. The partial line and the nickname fit in the connection's
  state, so a fresh instance per event is enough, and a crash ends one
  connection, not the room.
- The buffer is kept in the state one char per byte (Latin-1), because the
  state holds text and a read can end inside a UTF-8 character. Lines are
  decoded as UTF-8 only once they are whole.
- A line over 4 KB closes the connection. Cutting it would hide the error;
  keeping it would let one client fill the state.
- Control characters other than tab are taken out of each line, so a client
  cannot send escape codes to the other terminals.
- The sender gets its own message back from the topic. That shows it went
  through, and keeps the handler to one publish.
- `/who` reads the table `here` and checks each row with
  `connections.remote`. A row is left behind only when the server stops
  without close events, and `/who` removes it.
- Only messages are stored. Joins, leaves and renames are live only.

## Unfinished

- Nicknames need not be unique.
- No history on join: a client sees what is said after it signs in.
