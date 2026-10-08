# tric on AWS

One `tofu apply` installs tric into an AWS account. Apps are then served at `https://<app>.<domain>`. Nothing runs
while idle: the only standing costs are the domain's Route 53 zone and what the bucket stores.

## What it installs

- **The router.** These are two functions, built from one package and running as one role:
  - `tric-route` owns the function URL. CloudFront is its only caller, and the router refuses any request without
    CloudFront's secret, `X-Tric-Origin`.
  - `tric-events` takes only events. Cron arrives from EventBridge Scheduler. Delivery events arrive through its
    `outbox` alias, which retries twice, keeps an event for up to 6 hours, and then writes it to the bucket under
    `aws/lambda/async/` (the dead letters).
- **serve** runs the apps, with one Lambda tenant per app. It has no URL, and only the router can invoke it.
- **One bucket**, versioned:
  - `apps/<app>/…` holds the releases, components and state;
  - `native/<app>/…` holds the compiled code;
  - the lifecycle rules expire noncurrent versions after a day, and dead letters after 14 days.
- **CloudFront and DNS.** CloudFront serves `*.<domain>` with a wildcard certificate from ACM, and Route 53 aliases
  point the domain at it.
- **A $5 monthly budget**, alerting at $4. It is created only when `alert_email` is set.

## Who can do what

- **An app** gets credentials from the router, and those credentials reach only its own data:
  - it can read and write its names, its values and `native/<app>/*`;
  - it can read the rest of `apps/<app>/*`;
  - it gets nothing at all outside its own app.
- **serve's own role** has no storage access. It can write its logs and invoke the router's `outbox` alias, and
  nothing else, so an app that escapes the sandbox holds only its own tenant's credentials.
- **The router** is the trusted core and runs no app code. It can:
  - assume the app role, which trusts only the router;
  - read `apps/*/current`;
  - invoke serve.
- **Scheduler** can invoke `tric-events` unqualified (cron only). It can never invoke it as `outbox`.
- **Requests that bypass CloudFront** are refused. CloudFront replaces any `X-Tric-Origin` a viewer sends. The
  router replaces every `Forwarded`, so `for=_cron` and `for=_tric` come only from tric.

There are three things to know:
- **The state file holds the origin secret** (`terraform.tfstate`, kept locally and git-ignored). Anyone with the
  secret can reach the router directly and claim any client address. They still cannot reach another app's data, or
  forge cron or tric's own calls.
- **Apps share cookies across `*.<domain>`**, so use a domain for tric alone.
- **Shared between apps:** the account's Lambda concurrency, the log groups and the router. One app that is busy, or
  slow to answer, can use up the concurrency, and the others are then throttled.

## Install

You need:
- an AWS account;
- a domain whose hosted zone is in Route 53;
- Docker;
- an arm64 host (such as Apple silicon) to build the package, which runs on Graviton.

All commands run from the repository's root.

1. Build the package, `dist/tric.zip`:

   ```bash
   docker compose run --rm package
   ```

2. Export credentials for the account. OpenTofu and `tric deploy` both read them from the environment:

   ```bash
   eval "$(aws configure export-credentials --profile <profile> --format env)"
   ```

3. Install. `alert_email` is optional, and passing it adds the budget:

   ```bash
   export TF_VAR_domain=example.com TF_VAR_alert_email=you@example.com
   docker compose run --rm tofu init
   docker compose run --rm tofu apply
   ```

   The region is `us-west-2` unless you add `-var region=<region>`. The first apply waits a few minutes for the
   certificate and the distribution.

After a new package, run `apply` again: it updates the three functions.

## Deploy an app

`tric deploy` writes the app's component and release to the bucket, then syncs its cron to Scheduler. Install `tric`
with `cargo install --locked --path .` in a checkout, then take its settings from the install's outputs:

```bash
for k in TRIC_BUCKET TRIC_SCHEDULES TRIC_EVENTS TRIC_SCHEDULER_ROLE AWS_REGION; do
  export "$k=$(docker compose run --rm -T tofu output -raw "$k")"
done
tric deploy path/to/app    # then https://<app>.example.com
```

The app's name must be a DNS label. On AWS, a cron expression may restrict the day of the month or the day of the
week, but not both, because Scheduler can't express both. tric refuses it everywhere, so an app runs the same in
`tric dev`.

## Checks

`docker compose run --rm tofu test` checks the module's isolation properties against mock providers, and reaches
nothing on AWS:
- serve's tenancy;
- the one function URL;
- serve's and the router's permissions;
- the app role's trust;
- Scheduler's target;
- the outbox's async config;
- the origin secret;
- the lifecycle rules.

## Remove

```bash
docker compose run --rm tofu destroy
```

This deletes everything, including the bucket and every app's data.
