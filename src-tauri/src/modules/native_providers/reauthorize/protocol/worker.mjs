// Private NDJSON worker. Raw upstream logs, URLs and response bodies never leave this process.
import { createRequire } from "node:module";
import {
  CookieJar,
  ProtocolClient,
  loginChatgptWeb,
  runCodexOauth,
  resumeCodexOauth,
  buildSub2apiOauthExport,
  isExpiredCheckpointError,
} from "./vendor/tosub2/src/protocol-login.mjs";
import { TlsFingerprintTransport } from "./vendor/tosub2/src/tls-transport.mjs";

const require = createRequire(import.meta.url);
const CALLBACK = "http://localhost:1455/auth/callback";
const CLIENT = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODES = new Set([
  "DEPENDENCIES_MISSING",
  "INVALID_INPUT",
  "INVALID_CHECKPOINT",
  "IDENTITY_MISMATCH",
  "LOGIN_REJECTED",
  "TOTP_REJECTED",
  "EMAIL_REQUIRED",
  "PHONE_REQUIRED",
  "PROFILE_REQUIRED",
  "SECURITY_CHECK",
  "NETWORK_FAILED",
  "RATE_LIMITED",
  "ACCOUNT_UNAVAILABLE",
  "STEP_TIMEOUT",
  "TASK_TIMEOUT",
  "PROTOCOL_FAILED",
]);
const fail = (code) => {
  throw Object.assign(new Error(code), { code });
};
const claims = (token) => {
  try {
    return JSON.parse(Buffer.from(token.split(".")[1], "base64url").toString());
  } catch {
    return {};
  }
};
function failure(error) {
  if (CODES.has(error?.code)) return error.code;
  const text = String(error?.message || "");
  if (
    /curl_cffi|cannot find module|no module named|Python.*(?:unavailable|环境)|未找到.*Python/i.test(
      text,
    )
  )
    return "DEPENDENCIES_MISSING";
  if (/account_deactivated|account_deleted|account.*suspended/i.test(text))
    return "ACCOUNT_UNAVAILABLE";
  if (/HTTP 429|rate.limit/i.test(text)) return "RATE_LIMITED";
  if (
    /AUTH_REQUEST_BLOCKED|PROXY_RISK_CONTROL|security.check|Cloudflare|Turnstile|SESSION_SELECTION_INVALID|安全校验/i.test(
      text,
    )
  )
    return "SECURITY_CHECK";
  if (
    /PROXY_CONNECTION_RETRY|timed? out|TLS.*(?:fail|error)|SSL|connect.*(?:fail|refus)|ECONN|resolve.*host/i.test(
      text,
    )
  )
    return "NETWORK_FAILED";
  return "PROTOCOL_FAILED";
}
function endpoint(value) {
  const url = new URL(value);
  if (url.username || url.password || url.search || url.hash)
    fail("INVALID_INPUT");
  if (!["http:", "https:"].includes(url.protocol)) fail("INVALID_INPUT");
  return url.origin;
}
function verifyCallback(oauth) {
  let url;
  try {
    url = new URL(oauth.callbackUrl);
  } catch {
    fail("IDENTITY_MISMATCH");
  }
  if (
    url.origin !== "http://localhost:1455" ||
    url.pathname !== "/auth/callback" ||
    url.username ||
    url.password ||
    url.hash
  )
    fail("IDENTITY_MISMATCH");
  const params = url.searchParams;
  if (
    params.getAll("state").length !== 1 ||
    params.get("state") !== oauth.state ||
    params.getAll("code").length !== 1 ||
    !params.get("code") ||
    params.has("error")
  )
    fail("IDENTITY_MISMATCH");
}
function verifiedAccount(data, email) {
  const account = data.accounts?.length === 1 && data.accounts[0];
  if (!account || account.platform !== "openai") fail("IDENTITY_MISMATCH");
  const tokens = account.credentials || {},
    id = claims(tokens.id_token),
    access = claims(tokens.access_token);
  const identity = id["https://api.openai.com/auth"] || {},
    fallback = access["https://api.openai.com/auth"] || {};
  const accountId = identity.chatgpt_account_id || fallback.chatgpt_account_id;
  if (
    (id.email || access.email || "").toLowerCase() !== email.toLowerCase() ||
    !accountId ||
    !tokens.access_token ||
    !tokens.refresh_token
  )
    fail("IDENTITY_MISMATCH");
  // Do not use the upstream exporter's sid claim as the ChatGPT account id.
  tokens.chatgpt_account_id = accountId;
  tokens.email = email;
  account.extra = {
    ...account.extra,
    account_id: accountId,
    chatgpt_account_id: accountId,
  };
  return account;
}

export async function authorize(input, options = {}) {
  if (
    !input ||
    typeof input.email !== "string" ||
    !input.email.includes("@") ||
    typeof input.password !== "string" ||
    !input.password ||
    typeof input.two_factor !== "string"
  )
    fail("INVALID_INPUT");
  const origins = options.origins || {
    auth: "https://auth.openai.com",
    chatgpt: "https://chatgpt.com",
  };
  const authBase = endpoint(origins.auth),
    chatgptBase = endpoint(origins.chatgpt);
  if (
    options.origins &&
    ![authBase, chatgptBase].every((v) => /^http:\/\/127\.0\.0\.1:\d+$/.test(v))
  )
    fail("INVALID_INPUT");
  if (input.proxy) {
    let url;
    try {
      url = new URL(input.proxy);
    } catch {
      fail("INVALID_INPUT");
    }
    if (
      !["http:", "https:", "socks5:", "socks5h:"].includes(url.protocol) ||
      !url.hostname ||
      url.search ||
      url.hash
    )
      fail("INVALID_INPUT");
  }
  const emit = options.emit || (() => {}),
    started = Date.now();
  let stage = "preparing",
    stageAt = started,
    verifiedWeb = null,
    client;
  const output = (record) => emit(record);
  const progress = (next) => {
    if (stage !== next) {
      stage = next;
      stageAt = Date.now();
      output({ type: "progress", stage, elapsed_ms: Date.now() - started });
    }
  };
  const original = {
    log: console.log,
    warn: console.warn,
    error: console.error,
  };
  console.log = (...args) => {
    const message = String(args[0] || "");
    if (/^\[3\/5\] Password/.test(message)) progress("password");
    else if (/^\[mfa\]/.test(message)) progress("two_factor");
    else if (/^\[cloudflare\]/.test(message)) progress("security_check");
    else if (/^\[5\/5\]/.test(message)) progress("workspace");
  };
  console.warn = console.error = () => {};
  let transport,
    timer,
    flowPromise,
    closed = false;
  const timeout = Math.max(
    1000,
    Math.min(300000, options.timeoutMs || input.timeout_ms || 120000),
  );
  const stepTimeout = options.stepTimeoutMs || 45000;
  const checkpoint = async () => {
    if (!verifiedWeb || !client) return;
    const value = {
      version: 1,
      stage: "email_verified",
      email: input.email,
      updated_at: Date.now(),
      cookies: client.jar.toJSON(),
      web: verifiedWeb,
    };
    if (JSON.stringify(value).length > 262144) fail("INVALID_CHECKPOINT");
    output({ type: "checkpoint", checkpoint: value });
  };
  const rl = {
    question: async (prompt) => {
      if (/password/i.test(prompt)) fail("LOGIN_REJECTED");
      if (/phone/i.test(prompt)) fail("PHONE_REQUIRED");
      if (/2fa|totp/i.test(prompt)) fail("TOTP_REJECTED");
      fail("EMAIL_REQUIRED");
    },
  };
  try {
    require.resolve("jsdom");
    transport = options.transportFactory
      ? options.transportFactory()
      : new TlsFingerprintTransport({
          enabled: true,
          profile: "chrome146",
          verbose: false,
          maxProxySessionAttempts: 1,
          sameProxyRiskRetries: 1,
          sameProxyRiskRetryDelayMs: 500,
          cloudflareSolverTimeoutMs: 15000,
          sentinelTimeoutMs: 30000,
        });
    const rawFetch = transport.fetch.bind(transport);
    transport.fetch = async (url, request) => {
      if (closed) fail("TASK_TIMEOUT");
      const target = new URL(url);
      const allowed = options.origins
        ? [authBase, chatgptBase].includes(target.origin)
        : target.protocol === "https:" &&
          (target.hostname === "chatgpt.com" ||
            target.hostname === "openai.com" ||
            target.hostname.endsWith(".openai.com"));
      if (!allowed || target.username || target.password)
        fail("IDENTITY_MISMATCH");
      if (
        /\/api\/accounts\/(create_account|password\/add)|\/mfa\/(enroll|user\/activate_enrollment)/.test(
          target.pathname,
        )
      )
        fail("PROFILE_REQUIRED");
      return rawFetch(url, request);
    };
    const generate = transport.generateSentinelTokens.bind(transport);
    transport.generateSentinelTokens = async (args) => {
      const previous = stage;
      progress("security_check");
      const tokens = await generate(args);
      progress(previous);
      return tokens;
    };
    const solve = transport.solveCloudflareChallenge.bind(transport);
    transport.solveCloudflareChallenge = async (...args) => {
      const previous = stage;
      progress("security_check");
      const result = await solve(...args);
      if (result?.ok) progress(previous);
      return result;
    };
    const expired = new Promise((_, reject) => {
      timer = setInterval(() => {
        const code =
          Date.now() - started >= timeout
            ? "TASK_TIMEOUT"
            : Date.now() - stageAt >= stepTimeout
              ? "STEP_TIMEOUT"
              : null;
        if (code) reject(Object.assign(new Error(code), { code }));
      }, 250);
    });
    const flow = async () => {
      progress("connecting");
      // No session rotation or direct fallback: use exactly the route selected by the app.
      await transport.configure(input.proxy || null, { force: true });
      const saved = input.checkpoint;
      let useSaved =
        saved &&
        saved.version === 1 &&
        saved.stage === "email_verified" &&
        saved.email?.toLowerCase() === input.email.toLowerCase() &&
        Array.isArray(saved.cookies) &&
        saved.web &&
        Number.isFinite(saved.updated_at) &&
        Date.now() - saved.updated_at < 12 * 60 * 60 * 1000 &&
        saved.updated_at <= Date.now();
      for (let attempt = 0; attempt < 2; attempt++) {
        client = new ProtocolClient({
          verbose: false,
          transport,
          jar: useSaved ? CookieJar.fromJSON(saved.cookies) : new CookieJar(),
        });
        if (useSaved) {
          progress("resuming");
          verifiedWeb = saved.web;
        } else {
          progress("login");
          verifiedWeb = await loginChatgptWeb(client, {
            chatgptBase,
            authBase,
            email: input.email,
            rl,
            password: input.password,
            totpSecret: input.two_factor,
          });
        }
        await checkpoint();
        progress("oauth");
        let oauth;
        try {
          const settings = {
            authBase,
            rl,
            codexClientId: CLIENT,
            codexRedirectUri: CALLBACK,
            saveCheckpoint: checkpoint,
          };
          oauth = useSaved
            ? await resumeCodexOauth(client, settings, {
                ...saved,
                stage: "email_verified",
              })
            : await runCodexOauth(client, settings);
        } catch (error) {
          if (useSaved && isExpiredCheckpointError(error)) {
            useSaved = false;
            verifiedWeb = null;
            output({ type: "checkpoint", checkpoint: null });
            await transport.configure(input.proxy || null, { force: true });
            continue;
          }
          throw error;
        }
        verifyCallback(oauth);
        progress("token_exchange");
        const exported = await buildSub2apiOauthExport({
          authBase,
          callbackUrl: oauth.callbackUrl,
          codeVerifier: oauth.codeVerifier,
          clientId: CLIENT,
          redirectUri: CALLBACK,
          accountName: input.email,
          transport,
          cookie: client.jar.headerFor(authBase + "/oauth/token"),
        });
        const account = verifiedAccount(exported.data, input.email);
        // Keep only a reusable web session, never a spent OAuth callback or code.
        await checkpoint();
        return account;
      }
      fail("INVALID_CHECKPOINT");
    };
    flowPromise = flow();
    const account = await Promise.race([flowPromise, expired]);
    output({ type: "result", account });
    return account;
  } catch (error) {
    const code = failure(error);
    if (code === "IDENTITY_MISMATCH")
      output({ type: "checkpoint", checkpoint: null });
    output({
      type: "error",
      code,
      stage,
      elapsed_ms: Date.now() - started,
      waiting: [
        "EMAIL_REQUIRED",
        "PHONE_REQUIRED",
        "PROFILE_REQUIRED",
        "SECURITY_CHECK",
      ].includes(code),
      retryable:
        ["NETWORK_FAILED", "RATE_LIMITED"].includes(code) &&
        stage !== "token_exchange",
    });
    throw Object.assign(new Error(code), { code });
  } finally {
    closed = true;
    clearInterval(timer);
    await transport?.close();
    // Closing the transport settles its outstanding requests. Keep raw upstream
    // logs suppressed while the cancelled flow unwinds.
    if (flowPromise) await flowPromise.catch(() => {});
    Object.assign(console, original);
  }
}

if (process.argv.includes("--abv-protocol-worker")) {
  let terminal = false;
  const emit = (record) => {
    if (record.type === "result" || record.type === "error") terminal = true;
    process.stdout.write(JSON.stringify(record) + "\n");
  };
  try {
    let text = "";
    for await (const part of process.stdin) {
      text += part;
      if (text.length > 524288) fail("INVALID_INPUT");
    }
    await authorize(JSON.parse(text), { emit });
  } catch (error) {
    if (!terminal)
      emit({
        type: "error",
        code: "INVALID_INPUT",
        stage: "preparing",
        waiting: false,
        retryable: false,
      });
    process.exitCode = 1;
  }
}
