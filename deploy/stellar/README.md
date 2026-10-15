# Stellar AI deployment

The private repository's default and production branch is `main`. A push to
`main`, or a manual run of **Stellar AI Build and Deploy** on `main`, runs three
jobs in parallel: TypeScript, deployment-safety and workflow checks; native Rust
Clippy and focused regression tests; and the amd64 Linux image build, which runs
isolated API smoke checks before publishing a commit-tagged image to GHCR.
Production deploys the immutable digest only after all three jobs pass, so a
published image that failed a check is never deployed.

The image build caches compiled Rust dependencies as a separate layer (cargo-chef)
in the `buildcache` tag of the GHCR package, so a commit that leaves
`Cargo.toml`/`Cargo.lock` unchanged only recompiles this crate. The first run after
a dependency change rebuilds that layer and takes noticeably longer.
The upstream Pages workflow is manual-only; the custom workflow replaces the
upstream push-to-main CI. Upstream PR checks remain available.

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
usage is discarded. The old image does not enforce custom limits or expose the
portal. Full snapshot restoration is a manual recovery operation, only when
needed and explicitly authorized. Future destructive migrations must revisit
this strategy. Backups and old images are retained; no broad Docker cleanup runs.

## Local validation

```sh
python3 -m unittest discover -s deploy/stellar -p 'test_*.py'
actionlint .github/workflows/stellar-deploy.yml
```

`smoke.py <base-url>` uses `SMOKE_ADMIN`, creates and edits a test token, and must
run only against an isolated disposable instance. The workflow supplies one.
