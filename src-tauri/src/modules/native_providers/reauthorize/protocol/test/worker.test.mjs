import assert from "node:assert/strict";
import test from "node:test";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { authorize } from "../worker.mjs";
import { startMock } from "./mock-server.mjs";

process.env.NODE_ENV = "test";
process.env.TOSUB2_TEST_SENTINEL_TOKEN = "mock-password-verify-challenge";
const input = {
  email: "mfa-test@example.com",
  password: "local-test-password",
  two_factor: "JBSWY3DPEHPK3PXP",
};
async function fixture(controls, run) {
  const server = await startMock(controls);
  try {
    await run({ origins: { auth: server.base, chatgpt: server.base } }, server);
  } finally {
    await server.close();
  }
}
const isCode = (code) => (error) => error.code === code;

test("password, TOTP, workspace and OAuth use real curl transport; checkpoint resumes without password", async () => {
  await fixture({}, async (options, server) => {
    const records = [];
    const result = await authorize(input, {
      ...options,
      emit: (r) => records.push(r),
    });
    assert.equal(result.credentials.chatgpt_account_id, "real-account-id");
    assert.notEqual(result.credentials.chatgpt_account_id, "mock-account-id");
    assert.equal(result.credentials.email, input.email);
    const checkpoint = records.findLast(
      (r) => r.type === "checkpoint",
    ).checkpoint;
    assert.equal(checkpoint.stage, "email_verified");
    assert.equal(checkpoint.oauth, undefined);
    const resumed = [];
    await authorize(
      { ...input, checkpoint },
      { ...options, emit: (r) => resumed.push(r) },
    );
    assert(resumed.some((r) => r.stage === "resuming"));
    assert(!resumed.some((r) => r.stage === "password"));
    assert.equal(
      (await (await fetch(server.base + "/__test/state")).json()).passwordCount,
      1,
    );
    const publicRecords = records.filter(
      (r) => r.type === "progress" || r.type === "error",
    );
    const text = JSON.stringify(publicRecords);
    for (const secret of [
      input.password,
      input.two_factor,
      "mock-session",
      "mock-refresh-token",
    ])
      assert(!text.includes(secret));
  });
});
test("password rejection and extra email verification terminate without prompting", async () => {
  await fixture({}, async (options) => {
    await assert.rejects(
      authorize({ ...input, password: "wrong-password" }, options),
      isCode("LOGIN_REJECTED"),
    );
    await assert.rejects(
      authorize({ ...input, two_factor: "NB2W45DFOIZAQWER" }, options),
      isCode("TOTP_REJECTED"),
    );
    const records = [];
    await assert.rejects(
      authorize(
        { ...input, email: "email-mfa@example.com" },
        { ...options, emit: (r) => records.push(r) },
      ),
      isCode("EMAIL_REQUIRED"),
    );
    assert.equal(records.at(-1).waiting, true);
    assert.equal(records.at(-1).retryable, false);
  });
});
test("rate limiting keeps reusable web checkpoint and resumes on explicit retry", async () => {
  const controls = { oauthRateLimited: true };
  await fixture(controls, async (options) => {
    const records = [];
    await assert.rejects(
      authorize(input, { ...options, emit: (r) => records.push(r) }),
      isCode("RATE_LIMITED"),
    );
    assert.equal(records.at(-1).retryable, true);
    const checkpoint = records.findLast(
      (r) => r.type === "checkpoint",
    ).checkpoint;
    controls.oauthRateLimited = false;
    const resumed = [];
    await authorize(
      { ...input, checkpoint },
      { ...options, emit: (r) => resumed.push(r) },
    );
    assert(!resumed.some((r) => r.stage === "password"));
  });
});
for (const control of ["wrongState", "wrongIdentity"])
  test("rejects " + control + " and invalidates checkpoint", async () => {
    await fixture({ [control]: true }, async (options) => {
      const records = [];
      await assert.rejects(
        authorize(input, { ...options, emit: (r) => records.push(r) }),
        isCode("IDENTITY_MISMATCH"),
      );
      assert.equal(
        records.findLast((r) => r.type === "checkpoint").checkpoint,
        null,
      );
      assert(!records.some((r) => r.type === "result"));
    });
  });
test("step timeout closes transport with one terminal error", async () => {
  await fixture({ passwordDelay: 1500 }, async (options) => {
    const records = [];
    await assert.rejects(
      authorize(input, {
        ...options,
        stepTimeoutMs: 100,
        emit: (r) => records.push(r),
      }),
      isCode("STEP_TIMEOUT"),
    );
    assert.equal(records.filter((r) => r.type === "error").length, 1);
    assert(!records.some((r) => r.type === "result"));
  });
});
test("an unavailable fixed proxy cannot fall back to direct traffic", async () => {
  await fixture({}, async (options, server) => {
    await assert.rejects(
      authorize({ ...input, proxy: "socks5h://127.0.0.1:1" }, options),
    );
    assert.equal(
      (await (await fetch(server.base + "/__test/state")).json()).passwordCount,
      0,
    );
  });
});
test("CLI emits a safe terminal response for invalid stdin", () => {
  const child = spawnSync(
    process.execPath,
    [
      fileURLToPath(new URL("../worker.mjs", import.meta.url)),
      "--abv-protocol-worker",
    ],
    { input: '{"password":"private-secret"}', encoding: "utf8", timeout: 5000 },
  );
  assert.equal(child.status, 1);
  assert.deepEqual(JSON.parse(child.stdout), {
    type: "error",
    code: "INVALID_INPUT",
    stage: "preparing",
    waiting: false,
    retryable: false,
  });
  assert(!child.stdout.includes("private-secret"));
});
