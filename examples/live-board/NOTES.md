# live-board

## Schema

See migrations/. Add a numbered file for each change.

## Decisions

- Changes go through requests, not socket messages. A request has a status
  code, works from a tool and from curl, and is written before it is
  announced. The socket carries only what is live: the board snapshot,
  change events, presence and nudges.
- The board is the first message of every connection. The front end needs
  no separate fetch after a reconnect, and no event replay.
- A failed publish does not fail the request. The change is in the database,
  and the next connect sends it.
- Presence is a table, one row per connection, because a handler cannot list
  connections. A row outlives its connection only if the server stops with
  no close event, so each message refreshes `seen_at`, the list skips rows
  quiet for 90 seconds, and each connect deletes rows quiet for 180.
- A nudge is refused when the person has no board open, so the sender is
  told rather than left guessing.
- The display name is per connection. Two tabs may show different names;
  the presence list shows one per person.

## Unfinished

- No card order inside a lane other than creation time.
- No editing a card's title.
