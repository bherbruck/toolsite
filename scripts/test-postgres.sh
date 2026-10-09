#!/usr/bin/env bash
# Runs the Postgres tests: starts Postgres and MinIO in Docker on random
# local ports, runs every ignored test with "postgres" in its name against
# them, and removes both containers however the run ends.
#
#   scripts/test-postgres.sh           the ignored Postgres tests
#   scripts/test-postgres.sh --full    then the whole suite with
#                                      TOOLSITE_TEST_BACKEND=postgres
#
# `cargo test` alone never reaches either: the suite stays hermetic.
set -euo pipefail
cd "$(dirname "$0")/.."

docker=docker
if ! docker info >/dev/null 2>&1; then docker="sudo docker"; fi

# MinIO stopped publishing minio/minio to Docker Hub; Chainguard's build
# is public. Either can be swapped in here.
postgres_image="${POSTGRES_IMAGE:-postgres:16}"
minio_image="${MINIO_IMAGE:-cgr.dev/chainguard/minio:latest}"

suffix="$$-$RANDOM"
pg="toolsite-test-pg-$suffix"
minio="toolsite-test-minio-$suffix"
password="test-$(head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n')"

cleanup() { $docker rm -f "$pg" "$minio" >/dev/null 2>&1 || true; }
trap cleanup EXIT

$docker run -d --name "$pg" -e POSTGRES_PASSWORD="$password" \
  -p 127.0.0.1::5432 "$postgres_image" >/dev/null
$docker run -d --name "$minio" -e MINIO_ROOT_USER=toolsite -e MINIO_ROOT_PASSWORD="$password" \
  -p 127.0.0.1::9000 "$minio_image" server /data >/dev/null

pg_port=$($docker port "$pg" 5432/tcp | head -1 | sed 's/.*://')
minio_port=$($docker port "$minio" 9000/tcp | head -1 | sed 's/.*://')

# The image restarts the server once after initdb, so a single successful
# pg_isready is not enough: wait for a real query over TCP.
for _ in $(seq 1 60); do
  if $docker exec "$pg" psql -h 127.0.0.1 -U postgres -c 'select 1' >/dev/null 2>&1; then break; fi
  sleep 1
done
$docker exec "$pg" psql -h 127.0.0.1 -U postgres -c 'select 1' >/dev/null
for _ in $(seq 1 60); do
  if curl -sf "http://127.0.0.1:$minio_port/minio/health/ready" >/dev/null; then break; fi
  sleep 1
done
curl -sf "http://127.0.0.1:$minio_port/minio/health/ready" >/dev/null

export TOOLSITE_TEST_DATABASE_URL="postgres://postgres:$password@127.0.0.1:$pg_port/postgres?sslmode=disable"
export TOOLSITE_TEST_S3_ENDPOINT="http://127.0.0.1:$minio_port"
export TOOLSITE_TEST_S3_ACCESS_KEY_ID=toolsite
export TOOLSITE_TEST_S3_SECRET_ACCESS_KEY="$password"
# A Postgres site seals with the key from the environment. Exported for the
# whole run, so a file-mode test in the same binary as a Postgres one never
# sees the key appear halfway through.
export TOOLSITE_SECRET_KEY="$(head -c 32 /dev/urandom | base64)"
echo "$postgres_image on 127.0.0.1:$pg_port, minio on 127.0.0.1:$minio_port"

cargo test -- --ignored postgres

if [[ "${1:-}" == "--full" ]]; then
  TOOLSITE_TEST_BACKEND=postgres cargo test --no-fail-fast
fi
