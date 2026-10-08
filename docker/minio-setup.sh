#!/bin/sh
# Makes MinIO the bucket tric expects: `tric`, versioned, and a `router` user, whose sessions are the apps' credentials.
# MinIO's STS has no roles: a session has its user's policy, cut down by the session policy the router gives it.
set -e
until mc alias set m http://minio:9000 "$MINIO_ROOT_USER" "$MINIO_ROOT_PASSWORD" >/dev/null 2>&1; do sleep 1; done
mc mb --ignore-existing m/tric
mc version enable m/tric
# As on AWS: old versions expire, and so do the delete markers left with none.
mc ilm rule ls m/tric >/dev/null 2>&1 || mc ilm rule add --noncurrent-expire-days 1 --expire-delete-marker m/tric >/dev/null
mc admin policy create m router /setup/router-policy.json
mc admin user add m router router-local-only
mc admin policy attach m router --user router >/dev/null 2>&1 || true # already attached
