-- Schema `platform`, step 2: the catalog's second part. The project tree,
-- the record of an unfinished project move, every host label ever issued,
-- and one-time markers. Times are Unix seconds.

-- One row per project. A change to the tree reads every row and writes the
-- difference in one transaction, under an advisory lock, so two changes at
-- once never lose one another the way two whole-file rewrites would.
create table platform.projects (
    -- `ops`, `ops/yard`. Segments follow the slug rules.
    path text primary key,
    name text not null,
    created_at bigint not null,
    -- Only the permissions set on this project and above it apply inside.
    locked boolean not null default false,
    -- Paths this project had before a rename or a move, newest last.
    renamed_from text[] not null default '{}',
    -- public, authenticated or restricted; null follows the project above.
    gate text
);

-- An unfinished project move. At most one row: `one` is the primary key
-- and can only be true. Written before a move's first step, removed after
-- its last, and finished by whichever runner holds the relocation lock next.
create table platform.relocations (
    one boolean primary key default true check (one),
    from_path text not null,
    to_path text not null,
    started_at bigint not null
);

-- Every DNS label issued in subdomain mode, and the app it was issued to.
-- A label is never issued to another app, even once its app is removed:
-- the primary key is what makes that so, whichever runner assigns.
create table platform.host_labels (
    label text primary key,
    app text not null,
    issued_at bigint not null
);
create index host_labels_app on platform.host_labels (app);

-- One-time markers, such as old grants having been adopted.
create table platform.site_flags (
    name text primary key,
    value text not null,
    at bigint not null
);
