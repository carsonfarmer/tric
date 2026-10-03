# Keeping teams apart (fact sheet for Q64)

> Gathered 2026-10-02 by two research sub-agents from AWS, Google Cloud, Microsoft, Cloudflare, Backblaze and Tigris docs, plus the signed-release spike (branch `worktree-agent-ad67bf8860f1ba8e5`, commit `c7216e4`). (U) marks what the docs did not confirm. Only the spike's line count is measured; every other line count is an estimate. Prices are us-west-2.
> Answers decisions.md Q60 and Q64. Related reading: [state-scaling.md](state-scaling.md), [app-state-and-secrets.md](app-state-and-secrets.md).

## The question

torpor has no control plane. Deploys write the bucket directly (Q19), and the host reads it. Q60 asks for several teams to share an install, with no team able to reach another team's apps. That means something has to decide who may change app X, and there are three places that decision can live:

1. **The store.** IAM limits each team's credentials to that team's own prefix.
2. **The host.** Anyone may write, but the host ignores anything a deployer of X did not sign.
3. **A deploy endpoint that we run.** It is the only bucket writer, and it checks each caller against an owner list.

## What each protects

| Asset of app X | IAM prefixes | Signatures (the spike) | Deploy endpoint |
|---|---|---|---|
| Which code runs as X | Yes | Yes | Yes |
| X's secrets | Yes | Yes, once secrets are bound to X | Yes |
| X's KV data | Yes | **No.** Every writer can read and change `kv/`. Only a static IAM rule that keeps teams out of `kv/` closes this, and that is IAM anyway. | Yes |
| Which release X runs | Yes | **No.** Any writer can restore an older signed pointer, and its older secrets come back with it, which undoes a rotation (Q65's reason). | Yes |
| Domains | An object only the admin can write | Needs an admin signing key | An object only the admin can write |
| Deleting or corrupting X | Yes | **No** | Yes |
| What a mistake does | Fails open: a wrong policy grants access | Fails closed: an unknown key is refused | Fails open: an endpoint bug grants access |

Signatures can't stop the pointer replay. Storage you don't control can always show you old data that is signed correctly; this is the freeze attack. TUF's fix is an online signer that keeps re-signing a fresh timestamp, and an online signer is a control plane.

## Limits

### IAM, store by store

Granting access app by app runs into IAM's limits. Granting it team by team mostly doesn't, so the bucket layout decides which limit applies.

| Store | How a team is confined | One grant for every team? | What caps the number of teams | Apps per team |
|---|---|---|---|---|
| AWS S3, per-team prefix | One policy on `<dir>/${aws:PrincipalTag/team}/*`, plus `s3:prefix` for LIST | Yes (ABAC) | GitHub's OIDC token can't carry tags, so each team needs a role: 1,000 roles, raisable to 10,000 | Unlimited |
| AWS S3, per-app statements | One statement per app | No | A managed policy is 6,144 characters (fixed), about 30–60 apps; at most 25 policies per role | A few thousand at most, and fragile |
| GCS | One managed folder per team, because conditions can't confine LIST | No: the policies can't reference the caller's identity | Managed folders are unlimited; 1,500 principals per folder policy | Unlimited |
| Azure Blob | An ABAC condition on the blob path | Only when matching on the container name; the path form is undocumented (U) | 5,000 role assignments per subscription (fixed) | Unlimited |
| Cloudflare R2 | No prefix-scoped tokens. The only prefix limit is on temporary credentials (7 days at most) minted by a broker you run. | n/a | Needs option 3 | n/a |
| Tigris | A prefix policy per access key | No | Undocumented (U) | Unlimited |
| Backblaze B2 | Keys limited to a prefix | No | 100M keys | B2 has no conditional writes, so torpor can't run on it at all |

On AWS, anyone who can run `iam:TagRole` or `sts:TagSession` can move into another team. Only the admin may hold those permissions, and the `team` tag must not contain `/`.

### Signatures

- **Key limit.** Trusted keys reach the host through its config. A Lambda's whole environment is 4 KB (fixed), so `TORPOR_DEPLOYERS` fits about 70 keys. Past that, keys have to ship in the package or a layer, which means redeploying the host for each new team, or live in a bucket key list signed by a root key, which is more code.
- **Key handling.** Each team keeps a long-lived private key as a CI secret, in addition to its bucket credentials. A leaked key plus any bucket write access compromises all of that team's apps.
- **Portability.** It works on every store, including R2 and the local test servers.

### Deploy endpoint

- **Owner list.** The list is ours, kept in the admin object, so IAM's count limits don't apply to it.
- **Caller authentication.** On AWS, an IAM-auth Function URL identifies the caller's role, which again means a role per team (1,000, or up to 10,000). Alternatively, the endpoint verifies GitHub's OIDC tokens itself. That needs no cloud roles, but then only GitHub can deploy.
- **Uploads.** Lambda accepts at most 6 MB per request and a component may be 128 MB, so the endpoint has to issue presigned upload URLs.

## Costs

| | IAM prefixes | Signatures | Deploy endpoint |
|---|---|---|---|
| Rust lines | ~40–60 (estimate) | 109 (measured), plus ~20 to sign each app's secrets (estimate) | ~200 or more (estimate) |
| New crates | 0 | 0 | 0–2 (JWT, if it verifies OIDC itself) |
| OpenTofu | One policy and a role per team | A role per team for bucket writes, plus key config | A second function, plus a role per team or OIDC config |
| Adding a team | Add a role mapping repos to the team | Generate a key, store it in CI, and add it to every host's config | Add the team to the owner list, plus a role |
| At serve time | One more conditional GET per app per 5 s while it serves, about $0.21 a month per busy app per environment | A signature check per load | Nothing |
| Atomic multi-app release | Lost: each app has its own pointer | Kept | Kept |

## What IAM prefixes would change in the design

- **The bucket splits by team.** An index object holds `app → team` and `domains` (Q66). Only the admin writes it, and hosts recheck it as they recheck state today.
- **Each app gets its own pointer object,** `{release, secrets}` (Q65), which only that app's team can write. Releases and components move under the team's prefix, so components are no longer shared between teams.
- **Creating an app is an admin step:** add it to the index under a team. Moving an app to another team means copying its objects and changing the index.
- **Q45's ceilings apply per app instead of per install.** Those are 8 MB of state, and one deploy per second, the GCS write limit on one object. Each app's pointer is now its own object.
- **The compile trigger (Q58) needs blobs under their own top-level prefix,** such as `blobs/<team>/…`. S3 event filters only match a prefix and a suffix; EventBridge can match wildcards.
- **A team can still break its own apps.** The host keeps parsing everything strictly (Q53).
- **Local and single-team installs need no IAM at all.**

## Domains (Q66)

The map itself is not a constraint. It shares the admin object (8 MB, about 100k entries), and the admin writes it rarely. The real limits come from CloudFront and ACM, and they apply no matter who owns the map:

- **CloudFront:** 100 alternate domain names per distribution (adjustable; the maximum is unconfirmed), 200 distributions per account, and one certificate per distribution (fixed).
- **ACM:** 10 names per certificate by default, raisable to 100, and 2,500 certificates per account.
- **What that allows:** about 100 custom domains per distribution before you need a second distribution and certificate. A wildcard such as `*.apps.example.com` covers any number of subdomains with one name and one certificate, if the router maps `<app>.apps.example.com` to `app`. That routing is not built yet.
- **Many-domain option:** CloudFront's multi-tenant distributions (2025) are meant for many customer domains. Their limits and price are unconfirmed.
- **Admin work per domain:** each domain needs DNS, certificate validation, a CloudFront alias, and then a map entry. Teams can't add their own; letting them would need a proof of domain control, such as a DNS TXT check, in code.
