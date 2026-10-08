-- Tickets: upload URLs, settings links, inline uploads in progress, browser
-- upload URLs, provider sign-ins in flight, preview sign-ins and handoff
-- codes, so a ticket minted on one runner is redeemed on any other.
--
-- Nothing here opens anything by itself: `id_hash` is SHA-256 over the
-- kind and the ticket's id, never the id, and `payload` is sealed with a
-- key derived from TOOLSITE_SECRET_KEY, bound to `id_hash`. Expired rows
-- are swept whenever a ticket is minted.
create table state.tickets (
    id_hash text primary key,
    kind text not null,
    payload bytea not null,
    expires_at bigint not null
);
create index tickets_expires_at on state.tickets (expires_at);
