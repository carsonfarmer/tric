# tric on AWS

One `tofu apply` installs tric into an AWS account. Apps are then served at `https://<app>.<domain>`. Nothing runs
while idle: the only standing costs are the domain's Route 53 zone and what the bucket stores.

## What it installs

- **The router.** These are two functions, built from one package and running as one role:
  - `tric-route` owns the function URL. CloudFront is its only caller, and the router refuses any request without
    CloudFront's secret, `X-Tric-Origin`.
  - `tric-events` takes only events, each source's through an alias of its own. Cron arrives from EventBridge
    Scheduler at `cron`. Sockets' events arrive from API Gateway at `ws`. Delivery events arrive at `outbox`, which
    retries twice, keeps an event for up to 6 hours, and then writes it to the bucket under `aws/lambda/async/` (the
    dead letters).
- **serve** runs the apps, with one Lambda tenant per app. It has no URL, and only the router can invoke it.
- **WebSockets**, an API Gateway WebSocket API. CloudFront sends it a request that opens a socket, at the app's own
  URL. Its stage is throttled to `ws_rate` requests a second, 100 by default, with a burst of `ws_burst`, 200.
- **One bucket**, versioned:
  - `apps/<app>/…` holds the releases, components and state;
  - `native/<app>/…` holds the compiled code;
  - `ws/…` holds sockets' records and subscriptions, for the router alone;
  - the lifecycle rules expire noncurrent versions after a day, dead letters after 14 days, and `ws/` after a day.
- **CloudFront and DNS.** CloudFront serves `*.<domain>` with a wildcard certificate from ACM, and Route 53 aliases
  point the domain at it.

## Who can do what

- **An app** gets credentials from the router, and those credentials reach only its own data:
  - it can read and write its names, its values and `native/<app>/*`;
  - it can read the rest of `apps/<app>/*`;
  - it gets nothing at all outside its own app.
  - The app role caps every session: no app's credentials can write a release or a component, whatever the router asks.
- **serve's own role** has no storage access. It can write its logs and invoke the router's `outbox` alias, and
  nothing else, so an app that escapes the sandbox holds only its own tenant's credentials.
- **The router** is the trusted core and runs no app code. It can:
  - assume the app role, which trusts only the router;
  - read `apps/*/current`, and read and write `ws/*`;
  - invoke serve;
  - send to and close the sockets of its own API's stage.
- **Scheduler** can invoke `tric-events` as `cron` only, and **API Gateway** as `ws` only, from its own stage.
- **Requests that bypass CloudFront** are refused, and so are sockets: the router checks the secret when one opens.
  CloudFront replaces any `X-Tric-Origin` a viewer sends. The router replaces every `Forwarded`, so `for=_cron`,
  `for=_tric` and `for=_ws` come only from tric.

There are three things to know:
- **The state file holds the origin secret** (`terraform.tfstate`, kept locally and git-ignored). Anyone with the
  secret can reach the router directly, or open sockets at API Gateway's own URL, and claim any client address.
  They still cannot reach another app's data, or forge cron, tric's own calls or another socket's events.
- **Apps share cookies across `*.<domain>`**, so use a domain for tric alone.
- **Shared between apps:** the account's Lambda concurrency, the log groups, the router and the sockets' stage
  throttle. One app that is busy, or slow to answer, can use up the router's concurrency or the throttle, and the
  others are then throttled.
- **The router's concurrency is capped** at `route_concurrency`, 200 by default, which it reserves from the
  account's. A flood of client requests then holds at most 200 routers and the 200 serves they call, and further
  requests get 429, so about 600 of a 1,000 quota stays for cron and the outbox. Reserving costs nothing and keeps
  nothing warm. AWS keeps at least 100 unreserved, so an account still at a new account's limit of 10 needs
  `-var route_concurrency=-1`.

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

3. Install:

   ```bash
   export TF_VAR_domain=example.com
   docker compose run --rm tofu init
   docker compose run --rm tofu apply
   ```

   The region is `us-west-2` unless you add `-var region=<region>`. The first apply waits a few minutes for the
   certificate and the distribution.

After a new package, run `apply` again: it updates the three functions. The package is built with WebSockets.

## Deploy an app

`tric deploy` writes the app's component and release to the bucket, then syncs its cron to Scheduler. Install `tric`
with `cargo install --locked --path .` in a checkout, then take its settings from the install's outputs:

```bash
for k in TRIC_BUCKET TRIC_SCHEDULES TRIC_EVENTS TRIC_SCHEDULER_ROLE AWS_REGION; do
  export "$k=$(docker compose run --rm -T tofu output -raw "$k")"
done
tric deploy path/to/app    # then https://<app>.example.com
```

`TRIC_EVENTS` is the `cron` alias of `tric-events`, which the app's schedules invoke.

The app's name must be a DNS label. On AWS, a cron expression may restrict the day of the month or the day of the
week, but not both, because Scheduler can't express both. tric refuses it everywhere, so an app runs the same in
`tric dev`.

## Checks

`docker compose run --rm tofu test` checks the module's isolation properties against mock providers, and reaches
nothing on AWS:
- serve's tenancy;
- the one function URL;
- the router's reserved concurrency;
- serve's and the router's permissions;
- the app role's trust, and what it lets a session write;
- Scheduler's target, and API Gateway's;
- the outbox's async config;
- the sockets' stage throttle, and that no route sends a response;
- the origin secret;
- the lifecycle rules.

## Remove

```bash
docker compose run --rm tofu destroy
```

This deletes everything, including the bucket and every app's data.
