-- Schema `platform`, step 5: the latest turn claimed, one row per job.
--
-- `job_fires` kept a row per turn for a day, so one every-second job kept
-- 86,400 rows and an app with a hundred kept millions, and a turn older
-- than one claimed could still be claimed by a scheduler whose clock ran
-- behind. One row per job, holding the latest turn claimed, fires each turn
-- once and refuses any turn no later than it. The rows are claims, not
-- records of anything: the newest of each job's carries over.
create table platform.job_turns (
    app text not null,
    name text not null,
    due_at bigint not null,
    fired_at bigint not null,
    primary key (app, name)
);

insert into platform.job_turns (app, name, due_at, fired_at)
select app, name, max(due_at), max(fired_at) from platform.job_fires group by app, name;

drop table platform.job_fires;
