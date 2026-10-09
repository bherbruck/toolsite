-- Schema `platform`, step 6: generations that never repeat.
--
-- A runner caches an app's published files by its generation, so a
-- generation must never name two different publishes. Counted per row, it
-- did: a removal takes the row away, and the next app at the name started
-- again from 1, where another runner may still hold the old app's files.
-- One sequence for the whole site hands out every generation instead.

create sequence platform.generations;

-- Past every generation counted so far, removed apps' included.
select setval(
    'platform.generations',
    greatest(
        1,
        (select coalesce(max(generation), 0) from platform.pages),
        (select coalesce(max(generation), 0) from platform.removed_pages)
    )
);
