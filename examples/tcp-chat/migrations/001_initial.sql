-- Every chat line, for the page at the app root.
create table lines (
    id integer primary key,
    nick text not null,
    text text not null,
    at integer not null default (cast(strftime('%s', 'now') as integer))
);

-- Who is here: one row per signed-in connection, for /who. The handler
-- adds a row at sign-in and removes it on close.
create table here (
    conn text primary key,
    nick text not null
);
