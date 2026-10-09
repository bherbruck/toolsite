-- Schema `platform`: what is published, where, and how. This step holds the
-- catalog's first part: each page's and app's meta, notes and generation.
-- Times are Unix seconds, as everywhere in toolsite.

create schema platform;

-- One row per page or app with anything to keep. A slug with no row has
-- the default meta, no notes and generation 0, as a slug with no sidecars
-- has on files.
--
-- `meta` is the `PageMeta` serde shape. `json`, not `jsonb`: it keeps the
-- text as written, so a `\u0000` in a meta comes back as it went in, where
-- `jsonb` would refuse it.
create table platform.pages (
    slug text primary key,
    meta json not null default '{}',
    -- Publishes counted, so caches keyed on it never serve old content.
    generation bigint not null default 0,
    notes text,
    created_at bigint not null,
    updated_at bigint not null
);

-- What a removal took out of `pages`. Nothing destroys data: the rows stay
-- here, beside the files the trash keeps, so a removal can be undone.
create table platform.removed_pages (
    id bigserial primary key,
    slug text not null,
    meta json not null,
    notes text,
    generation bigint not null,
    created_at bigint not null,
    removed_at bigint not null
);
create index removed_pages_slug on platform.removed_pages (slug);
