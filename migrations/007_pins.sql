-- Apps whose tools a person wants listed as typed tools on their connector.
-- Every other app's tools stay one call away through app_tools and
-- call_app_tool, so a site with many apps does not flood a model's tool list.
create table pins (
    user_id    text not null references users(id),
    app        text not null,
    created_at integer not null,
    primary key (user_id, app)
);
