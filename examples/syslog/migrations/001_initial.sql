-- One row per datagram. A datagram with no <PRI> is kept whole in
-- message, with no facility and no severity.
create table logs (
    id integer primary key,
    received_at integer not null default (cast(strftime('%s', 'now') as integer)),
    -- Where the datagram came from, "ip:port".
    remote text not null,
    -- The sender's host name, or the remote's IP address when it gave none.
    host text not null,
    app text,
    -- 0 kernel, 1 user, ... 23 local7.
    facility integer,
    -- 0 emergency, 1 alert, 2 critical, 3 error, 4 warning, 5 notice,
    -- 6 info, 7 debug.
    severity integer,
    message text not null
);
create index logs_received_at on logs (received_at);
create index logs_host on logs (host, id);
