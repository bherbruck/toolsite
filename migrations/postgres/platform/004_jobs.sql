-- Schema `platform`, step 4: jobs, and the scheduled turns already fired.

-- One row per job. The job is `json` as its module shapes it (schedule,
-- path and how the last run went), as the other records are.
create table platform.jobs (
    app text not null,
    name text not null,
    job json not null,
    updated_at bigint not null,
    primary key (app, name)
);

-- A scheduled turn of a job, inserted before it runs. With two schedulers
-- on one database only the first insert of a turn succeeds, so a turn fires
-- exactly once. `due_at` is the turn's scheduled time, in Unix seconds.
create table platform.job_fires (
    app text not null,
    name text not null,
    due_at bigint not null,
    fired_at bigint not null,
    primary key (app, name, due_at)
);
