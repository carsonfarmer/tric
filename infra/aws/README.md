# tric on AWS

An install is one bucket, one Lambda function, CloudFront at `*.<domain>`, an EventBridge Scheduler group for the apps'
cron, a policy for whoever deploys, and a budget alert, all in one account. Everything that can be tagged is tagged
`tric = <name>`. Every command below runs from the repository's root, with `AWS_PROFILE` set to a profile that may
administer the account.

## Security first

- **The bucket is the boundary.** Hosts load native code from `native/` without checking it, as it cannot be checked,
  so whoever can write there can run code in the function, with its role: every app's state, and invoking itself. Only
  the function and the account's administrators can. The `deploy` policy writes only under `apps/`: a deployer controls
  every app's code, environment and state, but what they ship runs in the sandbox.
- **Apps run in a sandbox**: no files, no sockets, only their own environment variables, outbound HTTP only to the
  hosts their manifest lists and never to a private address, their own KV, and limits on time (10 s to answer, 300 s
  in all) and memory (256 MiB). The apps share one process, so Wasmtime is what keeps them apart: an app that broke
  out of it would have the function's role.
- **Environment variables are stored as they are**, in `apps/<app>/current`, which deployers and the function read,
  and one changed or removed stays in an old version for 7 days.
- **The Function URL is public**, as CloudFront's origin. A request that skips CloudFront reaches only what CloudFront
  would route it to, but sets its own `X-Forwarded-*`, so the `for` in an app's `Forwarded` is not proof of anything.
- **The apps share `<domain>` as one site**, so one can set a cookie that the browser sends to all of them. Apps that
  should not trust each other need installs of their own.
- **No concurrency is reserved by default**, as a new account's quota may be only 10, so nothing but the account's
  quota caps what a flood of requests costs: each instance that runs all day costs about $2 at 1769 MB. Set
  `concurrency` to cap it. The budget alert only emails.
- **Failures keep what failed**: an outbox's requests, headers and bodies included, in `failed/`, and an event whose
  invocation crashed in `aws/lambda/async/`.
- The bucket takes requests only over TLS. The module relies on S3's defaults for a new bucket: public access blocked,
  ACLs off, and encryption at rest with keys S3 manages.

## The zone, once per domain

The module serves from a public Route 53 zone that it reads but does not make, so destroying an install leaves the
domain as it was. If the domain has no zone in the account yet:

```bash
aws route53 create-hosted-zone --name <domain> --caller-reference "<domain>-$(date +%s)"
```

Then point the domain at the four name servers it prints: for a domain registered with Route 53,
`aws route53domains update-domain-nameservers --region us-east-1 --domain-name <domain> --nameservers Name=… Name=…`;
elsewhere, at its registrar. A zone costs $0.50 a month.

## Install

The install's settings go in `infra/aws/terraform.tfvars`, which git ignores, as it holds the alert's emails; the
variables are in [variables.tf](variables.tf). The region is us-west-2 unless `region` says otherwise.

```bash
printf '%s\n' 'domain = "<domain>"' 'budget = { emails = ["<you>"] }' > infra/aws/terraform.tfvars
docker compose run --rm release            # dist/tric.zip, for Lambda's arm64
docker compose run --rm tofu init
docker compose run --rm tofu apply
```

The build takes a few minutes, more on an x86 machine, where Docker emulates arm64; the apply about five more, most of
them CloudFront's. The install's state is `infra/aws/terraform.tfstate`, which only this checkout has: keep it until
the install is spun down, as without it tofu cannot. Before a second operator applies, move it to an S3 backend with
`use_lockfile = true`, so that each has the latest state and two applies cannot run at once.

Apps are then at `https://<app>.<domain>`. A resolver that looked a name up before the apply made its records may keep
that miss for up to 15 minutes, the zone's negative-caching time.

## Deploying

The CLI writes the bucket directly, so IAM is its only auth: attach the `deploy_policy` output to whoever deploys, or
deploy as an administrator. It takes its credentials, region and bucket from the environment, not from a profile:

```bash
eval "$(aws configure export-credentials --format env)"
export AWS_REGION=$(docker compose run --rm -T tofu output -raw region)
export TRIC_STORE=$(docker compose run --rm -T tofu output -raw store)
tric deploy path/to/app                    # prints the release's id; live within 5 s
tric releases <app>                        # newest first, marking the one it runs
tric release <app> <id>                    # runs an older one again
tric env <app> NAME=value OTHER=           # sets NAME and removes OTHER
```

`deploy` and `release` also make the app's cron schedules in the Scheduler group those of the release it runs.

## Upgrading

Pull the new tric, then build and apply as at install. Native code is keyed by Wasmtime's version and settings, so an
upgrade that changes either finds none for any app: each app is compiled again at its first request, which a large
component makes slow (seconds, for a JavaScript one), and its native code is written for the hosts that follow. What
the old build wrote is then unused, and goes with `aws s3 rm --recursive s3://<bucket>/native/`, which costs only
those compiles again.

## When things fail

- **Logs** are in CloudWatch, as JSON: `aws logs tail /aws/lambda/<name> --follow`. `logs.filter` sets their level.
- **An outbox** whose requests did not all succeed in their rounds is kept in `failed/<commit>`, its requests as the
  app made them, for the operator to look at or send again by hand.
- **An outbox or cron event** whose invocation crashed or timed out is tried twice more, and then kept in
  `aws/lambda/async/<name>/<yyyy>/<mm>/<dd>/…`, as Lambda records it. A status does not fail an invocation, so an event
  that ran and failed is in the logs, not here.

## Undoing a delete

The bucket keeps whatever is deleted or overwritten in it as an old version for 7 days. To bring back a key, list its
versions and copy the one you want over it:

```bash
aws s3api list-object-versions --bucket <bucket> --prefix apps/<app>/current
aws s3api copy-object --bucket <bucket> --key apps/<app>/current \
  --copy-source '<bucket>/apps/<app>/current?versionId=<version>'
```

An app's releases and components are never deleted, so an old `current` brought back runs as it did. A name's head can
be brought back the same way, while nothing writes the name: the large values it pins by version are still there.

## Costs

An idle install costs its storage and nothing else, plus $0.50 a month for the zone. A million requests cost about $0.83
on Lambda if each takes 27 ms at 1769 MB, and its free tier covers about 8 million such requests a month; $1 on
CloudFront, past its free 10 million a month; and the S3 requests they make. A host reads an app's `current` at most
once every 5 s. Each cron schedule that fires every minute is about 44,000 invocations a month, of Scheduler's free 14
million. Prices are AWS's list prices for us-east-1, which vary by region.

## Spin down

1. **The install.** Detach the `deploy` policy from whoever has it. The bucket goes only with everything in it, and only
   if `force_destroy` is applied first. Both commands need `dist/tric.zip`, as the plan reads it:

   ```bash
   docker compose run --rm tofu apply -var force_destroy=true
   docker compose run --rm tofu destroy -var force_destroy=true
   ```

   Deleting the Scheduler group deletes the apps' schedules in it. CloudFront takes a few minutes to disable before it
   can be deleted.

2. **Check nothing is left**, in both the install's region and us-east-1 (the certificate's):

   ```bash
   aws resourcegroupstaggingapi get-resources --region <region> --tag-filters Key=tric
   aws logs describe-log-groups --region <region> --log-group-name-prefix /aws/lambda/<name>
   ```

   A function that logged after its log group was deleted leaves an untagged group of Lambda's own, which
   `aws logs delete-log-group --log-group-name …` removes.

3. **The zone**, if no install will use the domain again. Destroy removes the install's records, which leaves the
   zone's own NS and SOA:

   ```bash
   aws route53 delete-hosted-zone --id <zone id>
   ```

   The domain stays registered, with name servers that answer for nothing until a new zone is made and named.
