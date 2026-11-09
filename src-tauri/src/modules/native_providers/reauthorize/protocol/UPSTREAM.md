# Protocol authorization engine

The files in `vendor/tosub2` are from [poxiao33/toSub2](https://github.com/poxiao33/toSub2),
commit `8548397e89bf80e508eda64a87e0d556d43abc84` (v1.7.1), under the MIT license.
The original license is retained at `vendor/tosub2/LICENSE`.

Local changes:

- Export protocol functions and run the original command-line entry only when executed directly.
- Always verify upstream TLS, including through loopback proxies.
- Disable environment proxy inheritance in the Python session so the application's selected route is authoritative.
- Explicit curl proxy options prevent `NO_PROXY` from silently bypassing that route.
- Bound Python dependency detection and Sentinel calls by the application's deadlines.
- Close private runtime input files before Node opens them, and use bounded reader
  threads for anonymous subprocess pipes on Windows, macOS and Linux.
- Route all SDK/Cloudflare broker requests, assets and iframe downloads through an
  exact HTTPS origin/endpoint policy. Reject private, reserved and mixed DNS answers,
  pin the checked destination through proxies while retaining TLS hostname checks,
  disable redirects and discard script-supplied routing/credential headers.
- After Cloudflare clearance, the trusted origin-root verification may observe an
  HTTP redirect to the login page without following it. Script-generated requests
  still reject redirect responses; endpoint and public-DNS restrictions also remain.

The application wrapper owns credentials, encrypted checkpoints, progress, cancellation,
timeouts, identity verification and retries. It does not launch the upstream console,
credential store, pool monitor, password setup or 2FA enrollment flows.

## Installation

Docker recipes install Node.js, jsdom and Python `curl_cffi==0.15.0` at image build
time. Login tasks do not install packages. Chromium remains available only for the
explicit legacy fallback.

For a desktop or unpackaged server, install Node.js 22+ and Python 3.10+,
run `npm ci --omit=dev --ignore-scripts` in this directory, and install
`vendor/tosub2/requirements.txt` in a Python virtual environment. Configure:

| Environment variable | Meaning |
| --- | --- |
| `ABV_CHATGPT_NODE` | Node executable; defaults to `node` on PATH. |
| `ABV_CHATGPT_AUTH_PYTHON` | Python executable in the virtual environment. |
| `ABV_CHATGPT_AUTH_NODE_MODULES` | Absolute path to this directory's `node_modules`. |
| `ABV_CHATGPT_AUTH_TIMEOUT_SECS` | Per-task time limit, default 120, range 30–300 seconds. |
| `ABV_CHATGPT_AUTH_ENGINE=browser` | Explicit fallback to the previous Chromium implementation. |

Passwords, 2FA secrets, cookies and results travel over private stdin/stdout pipes.
Built-in OAuth login and reauthorization use the server directly, including retries
of old failed imports. After authentication, the normal importer assigns an available
account node and verifies the credentials through it. API traffic and regular token
refresh use that node; a direct OAuth result never pins the account to DIRECT.
Switching the authentication route to DIRECT discards an old node checkpoint.

Checkpoints remain in the existing encrypted vault, tied to email and egress node;
only web sessions younger than 12 hours resume. Spent OAuth codes are never saved.
Temporary worker directories are owner-only on Unix and removed when a task ends.
The backend kills the worker process tree after cancellation, errors or completion.

Only connection failures and rate limits before token exchange retry automatically,
up to three attempts with backoff. Extra verification pauses the task for an explicit
retry. Saving new login material clears the old checkpoint. The existing backend
remains the sole owner of refresh-token rotation.

## Focused tests

Run `node --test test/worker.test.mjs` after installing the dependencies. These tests
exercise the actual protocol engine and Python transport against a local HTTP server,
including password/TOTP, workspace selection, OAuth state and identity validation,
checkpoint recovery, rejection, timeouts and fixed-proxy failure. They do not verify
that a real account can pass the official service's current security checks.

`python -m unittest discover -s test -p "test_*.py" -v` also runs the real Sentinel
and Cloudflare JavaScript runtimes with synthetic SDK inputs, without the worker's
test token shortcut. The same tests run on Windows, macOS and Linux in CI.
