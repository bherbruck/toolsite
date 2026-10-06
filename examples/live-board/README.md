# live-board

A shared board of cards that updates live in every open browser, with a
list of who is here and a nudge to one person. It shows the WebSocket
connection model: toolsite holds the sockets, the handler gets events.

## What it shows

- A declared socket: `[[socket]] path = "/live/ws"` in toolsite.toml. A
  path not declared refuses the upgrade.
- A handler built for the `app-with-connections` world, which exports
  `on-connection` next to `handle`. It gets `connect`, `message` and
  `close` events and calls `send`, `subscribe`, `publish` and `state-set`.
- Changes through ordinary requests. `/api/cards` writes to SQLite and then
  publishes to the topic `board`, so every open board updates, the one that
  made the change included.
- The board as the first message of every connection. A browser that
  reconnects gets the board as it is now and needs no replay.
- A display name kept in the connection's state, changed with a message.
- Presence on the topic `who`, sent on each connect and close.
- A nudge to one person through `user:<id>`. It reaches every board that
  person has open, and nobody else.
- A front end that reconnects by itself, waiting longer after each failure.
- The gate in front of the socket: `gate = "restricted"`, so a person with
  no grant cannot connect, the same as for any request to the app.
- One app tool, `add_card`. A card an assistant adds shows on every open
  board at once.
- "Connect an AI assistant" in the menu, with the app's connector link.

## Start it

```sh
toolsite init my-board --example live-board
cd my-board
toolsite deploy
```

Grant the people who may use it, then open it in two browsers.

## Use it from an assistant

Add `<site>/p/my-board/mcp` as a connector and sign in. Then ask, for
example, "Add a card to the board: order more pallet wrap."

## Messages

From the server, as JSON text:

- `{"type":"board","cards":[...],"me":{...}}`: first, on each connect.
- `{"type":"card","card":{...}}`: a card added or moved.
- `{"type":"deleted","id":7}`: a card removed.
- `{"type":"who","people":[{"id","name","tabs"}]}`: who is here.
- `{"type":"nudge","from":"Ana","text":"..."}`: to one person only.

From the browser: `{"type":"ping"}`, `{"type":"name","name":"Ana"}` and
`{"type":"nudge","to":"<user id>","text":"..."}`.

## Files

- `handler/src/lib.rs`: the routes and the connection events.
- `src/live.ts`: the socket hook, with reconnect and backoff.
- `src/App.tsx`: the board, presence and nudges.
- `migrations/001_initial.sql`: cards and presence.
- `toolsite.toml`: the gate, the socket and the tool.
