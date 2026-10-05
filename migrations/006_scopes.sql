-- Platform scopes: what an account may do to the platform from a point in
-- the project tree down. Unlike a grant's role, which the app reads and the
-- platform never interprets, a scope is one of three words the platform acts
-- on: viewer opens apps, editor also publishes under the prefix, admin also
-- decides access there.
--
-- prefix is a path in the project tree ('' is the root, 'ops', 'ops/yard'),
-- or an app's own path for a scope on one app. A person may hold many.

create table scopes (
    user_id    text not null references users(id),
    prefix     text not null,
    scope      text not null check (scope in ('viewer', 'editor', 'admin')),
    granted_by text,
    created_at integer not null,
    primary key (user_id, prefix)
);

create index scopes_by_prefix on scopes(prefix);

-- A grant on an app (the grants table, with its opaque role) counts as a
-- viewer scope on that app wherever the app sits; it is read live rather
-- than copied, so revoking a grant keeps meaning what it meant.
