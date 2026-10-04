# torpor on AWS

An install: the app, native-code and KV buckets, the serving and compile functions, CloudFront at `*.<domain>`, a role
per team, and a budget alert. Everything that can be tagged is tagged `torpor = <name>`. Every command below runs from
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
docker compose run --rm release            # builds dist/torpor.zip
docker compose run --rm tofu init
docker compose run --rm tofu apply
```

The install's state is `infra/aws/terraform.tfstate`, which only this checkout has: keep it until the install is spun
down, as without it tofu cannot.

Apps are then at `https://<app>.<domain>`. A resolver that looked a name up before the apply made its records may
keep that miss for up to 15 minutes, the zone's negative-caching time. The apps share `<domain>` as one site, so one
can set a cookie that the browser sends to all of them: teams that should not trust each other need installs of their
own.

## Using a team's role

The CLI takes its credentials from the `AWS_` variables, not from a profile. With a profile for the role in
`~/.aws/config` (its `role_arn` from the `team_roles` output, and a `source_profile`), a team member runs:

```sh
eval "$(aws configure export-credentials --profile <team> --format env)"
export AWS_REGION=<region> TORPOR_STORE=<store> TORPOR_NATIVE=<native>   # from the outputs
```

## Spin down

1. **The install.** The buckets go only with everything in them, and only if `force_destroy` is applied first. Both
   commands need `dist/torpor.zip`, as the plan reads it:

   ```sh
   docker compose run --rm tofu apply -var force_destroy=true
   docker compose run --rm tofu destroy -var force_destroy=true
   ```

   CloudFront takes a few minutes to disable before it can be deleted.

2. **Check nothing is left**, in both the install's region and us-east-1 (the certificate's):

   ```sh
   aws resourcegroupstaggingapi get-resources --region <region> --tag-filters Key=torpor
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
