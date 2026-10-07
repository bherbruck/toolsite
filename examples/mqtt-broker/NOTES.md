# mqtt-broker

## Schema

See migrations/. One row, `status`, written by the resident instance on
tick and read by `/api/status`. Nothing else is in the database: the broker
lives in memory.

## Decisions

- The broker is a fork of rumqttd 0.20.0 in `rumqttd/`, so MQTT behavior is
  upstream's. Change MQTT behavior upstream or in the fork with a line in
  FORK.md, never in the handler.
- The handler keeps a map from toolsite's connection ids to the broker's
  numeric keys. A connection the broker closes stays in the map until its
  close event arrives, so a late message for it is ignored, not misrouted.
- A write that `connections.send` refuses closes the connection: a lost
  write would cut an MQTT packet in half.
- The page connects as an MQTT client itself, so what it shows live is
  what MQTT delivers, not a copy made by the handler.

## Half-finished

- No access control per topic: every client of the app may publish and
  subscribe to every topic.
