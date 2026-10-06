-- A shared board of cards in three lanes. Everyone who may open the app
-- sees and changes every card: the board is shared on purpose.
create table cards (
    id integer primary key,
    title text not null check (length(title) between 1 and 200),
    lane text not null default 'todo' check (lane in ('todo', 'doing', 'done')),
    author_id text,
    author_email text,
    created_at integer not null default (cast(strftime('%s', 'now') as integer)),
    updated_at integer not null default (cast(strftime('%s', 'now') as integer))
);

-- Who has the board open: one row per open connection. The handler adds a
-- row on connect and removes it on close. A row outlives its connection
-- only when the server stops without a close, so seen_at, refreshed by
-- each message, lets the handler skip and clear rows that went quiet.
create table presence (
    conn text primary key,
    user_id text not null,
    name text not null,
    seen_at integer not null
);
create index presence_user on presence (user_id);
