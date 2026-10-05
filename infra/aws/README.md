# tric on AWS

An install: the app and KV buckets, the serving and compile functions, CloudFront at `*.<domain>`, a role
per team, and a budget alert. Everything that can be tagged is tagged `tric = <name>`. Every command below runs from
the repository's root, with `AWS_PROFILE` set to a profile that may administer the account.

## The zone, once per domain

The module serves from a public Route 53 zone that it reads but does not make, so destroying an install leaves the
domain as it was. If the domain has no zone in the account yet:

```sh
aws route53 create-hosted-zone --name <domain> --caller-reference "<domain>-$(date +%s)"
```

Then point the domain at the four name servers it prints: for a domain registered with Route 53,
`aws route53domains update-domain-nameservers --region us-east-1 --domain-name <domain> --nameservers Name=… Name=…`;
elsewhere, at its registrar. A zone costs $0.50 a month.

## Install

The install's settings go in `infra/aws/terraform.tfvars`, which git ignores, as it holds the alert's emails; the
variables are in [variables.tf](variables.tf). An account whose Lambda concurrency quota is under 122 has none to
reserve, so needs `concurrency = { serve = -1, compile = -1 }` too.

```sh
printf '%s\n' 'domain = "<domain>"' 'budget = { emails = ["<you>"] }' 'teams = ["<team>"]' > infra/aws/terraform.tfvars
docker compose run --rm release            # builds dist/tric.zip
docker compose run --rm tofu init
docker compose run --rm tofu apply
```

The install's state is `infra/aws/terraform.tfstate`, which only this checkout has: keep it until the install is spun
down, as without it tofu cannot. Before a second operator applies, move it to an S3 backend with `use_lockfile = true`,
so that each has the latest state and two applies cannot run at once.

Apps are then at `https://<app>.<domain>`. A resolver that looked a name up before the apply made its records may
keep that miss for up to 15 minutes, the zone's negative-caching time. The apps share `<domain>` as one site, so one
can set a cookie that the browser sends to all of them: teams that should not trust each other need installs of their
own.

## Teams

A team owns the apps named `<team>-…`: its role may publish, release and set the secrets of those, and otherwise only
list the app bucket's keys. To add one, add its name to `teams` in `terraform.tfvars` and apply again. A team's name is
1 to 38 of `a-z` and `0-9`, with no `-`, so no team's prefix is another's. Removing a name deletes the role, not the
team's apps.

The roles trust the account, so the account's own IAM policies say who may assume each: give the team's members
`sts:AssumeRole` on its ARN, from the `team_roles` output. The CLI takes its credentials from the `AWS_` variables, not
from a profile. With a profile for the role in `~/.aws/config` (its `role_arn`, and a `source_profile`), a team member
runs:

```sh
eval "$(aws configure export-credentials --profile <team> --format env)"
export AWS_REGION=<region> TRIC_STORE=<store> TRIC_NATIVE=true   # the region and store from the outputs
```

## Upgrading

Pull the new tric, then build and apply as at install. The compile function is updated first. Native code is keyed by
a hash of Wasmtime's version and settings, so after an upgrade that changes either, the hosts find none for any app:
each compiles an app itself the first time it loads it, which makes that request slow (a large JavaScript component
takes seconds), and asks the compile function for the new native code, which every later host loads. Publishing an app
again asks for it ahead of any request. The old native code stays in each release's folder until the app drops that
release.

## Garbage

Nothing needs collecting. An app keeps the release it serves and its 10 latest publishes, each in a folder of its own
with its component and native code, and each change to the app (a publish, a release or a secret) first deletes the
folders of the releases it no longer keeps. A folder thus goes at the change after the one that dropped its release, by
when hosts have mostly moved on, as each rechecks an app every 5 s. If the two changes land within 5 s, a host still
serving the dropped release goes on serving it while it has it loaded, but fails with a `500` if it has to load it
again, until its recheck. An app that never changes again keeps its last folders. A marker that asks for native code,
which a failed compile leaves, goes after a day, and KV data is never touched.

Deleting an app's `current` takes the app offline. The next change to the app starts its history again, in folders above
the old ones, and the change after it deletes the old ones. Do it only while nothing publishes to the app: a publish
that is still writing can leave its files in the next release's folder, which hosts then refuse, as they do not match
its hashes.

## Undoing a delete

The app bucket keeps whatever is deleted or overwritten in it as an old version for 7 days, which the operator can
bring back, though teams can neither read nor delete old versions. To bring back a key, list its versions and copy the
one you want over it:

```sh
aws s3api list-object-versions --bucket <store> --prefix apps/<app>/current
aws s3api copy-object --bucket <store> --key apps/<app>/current \
  --copy-source '<store>/apps/<app>/current?versionId=<version>'
```

Bring back a deleted `current` before anything changes the app: a change makes a new `current`, which bringing back the
old one undoes, and the change after it deletes the old folders. An older `current` brought back over a live one needs
the files of the folders it keeps, which come back the same way; the folders it does not keep go at the change after the
next. KV data has no old versions.

## Trust

- **Apps** run in a sandbox: no files, environment, sockets or private addresses, outbound HTTP only to the hosts their
  manifest lists, and limits on time and memory ([docs/apps.md](../../docs/apps.md#limits)). An app's KV stores are its
  own, and only the serving function reaches the KV bucket.
- **Teams** reach only their own apps, but can list every key in the app bucket, and so see other teams' app names and
  release ids. An app's secrets are stored as they are, readable by its team and by the hosts, and one that is changed
  or removed stays in an old version for 7 days, which only the operator can read.
- **The apps share `<domain>`** as one site, so one can set a cookie that the browser sends to all of them. Teams that
  should not trust each other need installs of their own.
- **The hosts trust nothing in the buckets but native code:** every other read is size-capped and parsed strictly, and
  every component and release is checked against its hash. Native code cannot be checked, and loading it runs it in the
  host, so only the compile function may write it. A team may write only its apps' `current` and their folders'
  `release` and `component`, while native code always ends in `.zst`, and the app bucket's policy refuses a key ending
  in `.zst` from anyone but the compile function, the operator included. The compile function names native code by the
  hash of the component it compiled, and a host loads only what is named by the component its app's `current` names. A
  team may delete its apps' native code, which costs only a compile.
- **The buckets** take requests only over TLS. The module relies on S3's defaults for new buckets: public access
  blocked, ACLs off, and encryption at rest with keys S3 manages.
- **The compile function** compiles every team's components, and can write native code for any app, so Wasmtime's
  compiler is the boundary: a component that exploited it there could run its code in every app. It also makes every
  team's native code in a few slots at a time, so a team that publishes many large components can delay the others',
  whose hosts compile their apps meanwhile: slower cold starts, but nothing breaks.
- **The operator**, with the account, can do anything, though writing native code takes changing the app bucket's
  policy first, which CloudTrail records.

## Costs

An idle install costs its storage and nothing else, plus $0.50 a month for the zone. A million requests cost about $0.83
on Lambda if each takes 27 ms, as a KV read did at 1769 MB, and its free tier covers about 8 million such requests a
month; $1 on CloudFront, past its free 10 million a month; and the S3 requests they make. A host reads an app's
`current` at most once every 5 s, and the KV calls' costs are in [docs/apps.md](../../docs/apps.md#what-kv-costs).
Prices are AWS's list prices for us-east-1, which vary by region. The reserved concurrency of `serve` caps what a flood
of requests can cost, at about $40 a day by default, and the budget alert emails at 80% of $5 a month of the account's
spend.

## Spin down

1. **The install.** The buckets go only with everything in them, and only if `force_destroy` is applied first. Both
   commands need `dist/tric.zip`, as the plan reads it:

   ```sh
   docker compose run --rm tofu apply -var force_destroy=true
   docker compose run --rm tofu destroy -var force_destroy=true
   ```

   CloudFront takes a few minutes to disable before it can be deleted.

2. **Check nothing is left**, in both the install's region and us-east-1 (the certificate's):

   ```sh
   aws resourcegroupstaggingapi get-resources --region <region> --tag-filters Key=tric
   aws logs describe-log-groups --region <region> --log-group-name-prefix /aws/lambda/<name>-
   ```

   A function that logged after its log group was deleted leaves an untagged group of Lambda's own, which
   `aws logs delete-log-group --log-group-name …` removes.

3. **The zone**, if no install will use the domain again. Destroy removes the install's records, which leaves the
   zone's own NS and SOA:

   ```sh
   aws route53 delete-hosted-zone --id <zone id>
   ```

   The domain stays registered, with name servers that answer for nothing until a new zone is made and named.
