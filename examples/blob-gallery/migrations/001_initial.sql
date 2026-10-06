-- One row per photo. The bytes live in the app's file store under
-- photos/<id>/full and photos/<id>/thumb; this table holds what a listing
-- needs. A row starts not ready, and becomes ready once the handler has seen
-- both files arrive, so a half-finished upload never shows in the gallery.

create table photos (
    id integer primary key,
    caption text not null default '',
    owner_id text not null,
    owner_email text not null,
    content_type text,
    size integer,
    ready integer not null default 0,
    created_at integer not null default (cast(strftime('%s', 'now') as integer))
);

create index photos_ready on photos(ready, id);
