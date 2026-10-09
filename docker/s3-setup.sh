#!/bin/sh
# Makes RustFS the bucket tric expects, `tric`, versioned, and a `router` user, whose sessions are apps' credentials.
# A session needs no role: it has its user's policy, cut down by the session policy the router gives it.
set -e
until rc alias set m http://s3:9000 "$RUSTFS_ACCESS_KEY" "$RUSTFS_SECRET_KEY" >/dev/null 2>&1 &&
  rc mb --ignore-existing m/tric >/dev/null 2>&1; do sleep 1; done
rc version enable m/tric
# As on AWS: old versions expire, and so do the delete markers left with none. Import replaces, so a re-run adds none.
rc ilm rule import m/tric /setup/lifecycle.json >/dev/null
rc admin policy create m router /setup/router-policy.json
rc admin user add m router router-local-only
rc admin policy attach m router --user router >/dev/null 2>&1 || true # already attached
