# Stellar AI deployment

The private repository's default and production branch is `main`. A push to
`main`, or a manual run of **Stellar AI Build and Deploy** on `main`, runs three
tracks in parallel: TypeScript, deployment-safety and workflow checks; native Rust
Clippy and focused regression tests; and the server build. The server build
compiles the frontend and the release binary in a `rust:1-slim-bookworm`
container, the same image `docker/Dockerfile` compiles in, so the binary matches
the glibc and libraries of the `debian:bookworm-slim` runtime. It then packages
them with `docker/Dockerfile --target prebuilt` and runs isolated API smoke checks
before publishing a commit-tagged image to GHCR. Production deploys the immutable
digest only after all three tracks pass, so a published image that failed a check
is never deployed.

The compile step caches every built dependency with `rust-cache`, so a
`Cargo.lock` change recompiles only the crates that changed rather than all of
them. The runtime package layer is cached in the `runtimecache` tag of the GHCR
package. Building `docker/Dockerfile` without a target still compiles everything
from source, as before, for local builds and releases.

The upstream Pages workflow is manual-only; the custom workflow replaces the
upstream push-to-main CI. PR checks (`ci.yml`) run on Linux only; macOS and
Windows builds are left to the release workflow.

## Deployment tags

After a verified deployment, a separate job, the only one with `contents: write`
(plus `actions: read` to list runs), runs `tag_deployments.py`. Every successful
`deploy` job of this workflow on `main` gets an annotated tag
`deploy-YYYYMMDD-HHMM-<short commit>` on the deployed commit, using the Beijing
time the deploy job finished, e.g. `deploy-20261018-1830-3875e5c`. Several
deployments a day get distinct, time-ordered tags.

Tag names come only from the run history, so the job is idempotent: each run also
tags any deployment from the last 30 days that has no tag yet, which backfills
deployments made before this job existed and repairs a failed tag job on the next
deployment. The current deployment's tag message records the image digest;
backfilled tags reference the commit-tagged image instead. Names never start
with `v`, which would trigger `release.yml`, and tags created with `GITHUB_TOKEN`
do not start other workflows. A failed tag job leaves the deployment in place.
List deployments with `git fetch --tags && git tag -l 'deploy-*'`.

## Credentials and notifications

- `DEPLOY_SSH_KEY`: dedicated SSH private key, restricted on the server to the
  root-owned `/usr/local/sbin/stellar-ai-deploy` command. It cannot open a shell,
  forward ports or upload files. Do not reuse a personal SSH key.
- `DEPLOY_KNOWN_HOSTS`: host key obtained through the already trusted SSH channel.
- Repository variable `DEPLOY_HOST`: production host address.
- GHCR uses the job-scoped `GITHUB_TOKEN`; the server uses a temporary Docker
  config directory and removes it after pulling. No persistent registry token.
- Email uses native GitHub Actions notifications, without SMTP credentials.
  In account Settings → Notifications → Actions, enable Email and disable
  “Only notify for failed workflows”. Check the default notification email.
  GitHub sends workflow status to the triggering user's configured email;
  this is not an arbitrary-recipient email service. A successful run includes
  server verification. Failed runs also notify when all results are enabled.
  Reference: https://docs.github.com/en/actions/concepts/workflows-and-actions/notifications-for-workflow-runs

## Server behavior

The server helper is installed manually from `deploy.py` as root-owned mode
0700; updating this file in Git does not automatically replace the privileged
helper. SSH accepts only this repository's `image@sha256:<digest>` plus a full
commit SHA, checks the image revision label, and uses an exclusive deployment
lock. GitHub workflow concurrency never cancels an in-progress deployment.
The expected `/health` version comes from the image's
`org.opencontainers.image.version` label, which the workflow reads from
`src-tauri/Cargo.toml`; an image without it is refused before the service stops.
Reinstall the helper after changing `deploy.py`.
The helper validates the current balance-billing schema (`balance_micros`,
`free_models`, `max_concurrency`) and portal response. `smoke.py` imports the same
portal validator and exercises it against the built server before publication;
legacy quota and allowlist fields are not part of this API contract.
It also requires `GET /api/model-registry` to load with at least one enabled
published model; public inference is closed when the registry cannot load. An
image that predates the registry has no such route (404 or the portal page), so
the check is skipped and the helper can be installed before that image is
replaced. On
the deployment that first creates `data/model_registry.json`, every exact priced
model and per-key free model must be published. If that cutover fails, the
previous image is restored and the newly created registry is moved into the
backup directory, so the next deployment migrates current data again. Later
deployments do not require priced models to stay published, because unpublishing
is a deliberate administrator action.

The existing `/opt/antigravity-manager/compose.yaml`, `.env`, Nginx, port mapping,
resource limits and data directory are reused. The old container's machine ID
is preserved for encrypted upstream proxy passwords. The image is pulled before
stopping the service. After a graceful stop (up to 60 seconds), a consistent
data archive including SQLite WAL files is saved under
`/opt/antigravity-manager-backups/<UTC timestamp>-<commit>/` with the old Compose
and environment files. Backup files contain secrets and are root-only.

There is a brief service interruption during backup and container replacement.
Health, exact running image, public HTTPS, portal HTML, unauthenticated denials,
admin access, schema migration and an existing enabled user's portal response
are checked. These checks do not generate billable upstream model calls.
They do not establish that all production model providers are available.

If verification fails, the previous Compose/image is restarted automatically.
The database is retained because this version uses additive migrations; no new
usage is discarded. Billing and portal behavior after rollback follow the restored
image's version. Full snapshot restoration is a manual recovery operation, only when
needed and explicitly authorized. Future destructive migrations must revisit
this strategy. Backups and old images are retained; no broad Docker cleanup runs.

## Local validation

```sh
python3 -m unittest discover -s deploy/stellar -p 'test_*.py'
actionlint .github/workflows/stellar-deploy.yml
```

`smoke.py <base-url>` uses `SMOKE_ADMIN`, creates and edits a test token, and must
run only against an isolated disposable instance. The workflow supplies one.
