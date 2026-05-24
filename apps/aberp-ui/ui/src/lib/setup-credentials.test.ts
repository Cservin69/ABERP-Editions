// PR-46α / session-62 — vitest pins on `setup-credentials.ts`. The
// composer is the load-bearing operator-facing contract for the
// first-run wizard's wire shape: a regression that mis-spelled one
// snake_case field would surface as a `400` from the backend, but
// the failure mode would be opaque ("technical_user_login is
// required" — but the operator DID type one). Pinning the composer
// catches that drift at the SPA layer.

import { describe, expect, it } from "vitest";

import {
  composeSetupCredentialsBody,
  validateSetupCredentials,
  type SetupCredentialsForm,
} from "./setup-credentials";

function fullForm(): SetupCredentialsForm {
  return {
    technicalUserLogin: "techuser-abc",
    technicalUserPassword: "pw-secret",
    xmlSignKey: "sk-very-long-random-string",
    xmlChangeKey: "ck-16-byte-blob!",
  };
}

describe("composeSetupCredentialsBody", () => {
  it("snake-cases every form field on the wire", () => {
    const body = composeSetupCredentialsBody(fullForm());
    // The four wire-shape fields MUST be present with exactly the
    // snake_case names the backend's serde-deserialiser expects.
    expect(body.technical_user_login).toBe("techuser-abc");
    expect(body.technical_user_password).toBe("pw-secret");
    expect(body.xml_sign_key).toBe("sk-very-long-random-string");
    expect(body.xml_change_key).toBe("ck-16-byte-blob!");
  });

  it("emits exactly four fields — no extras leak through", () => {
    const body = composeSetupCredentialsBody(fullForm()) as unknown as Record<
      string,
      unknown
    >;
    const keys = Object.keys(body).sort();
    expect(keys).toEqual([
      "technical_user_login",
      "technical_user_password",
      "xml_change_key",
      "xml_sign_key",
    ]);
  });

  it("preserves the verbatim secret strings without trim", () => {
    // Leading/trailing whitespace in a sign key is technically
    // invalid NAV input, but the composer MUST NOT silently strip
    // it — the operator gets a NAV-side 400 (per the live env) and
    // the inline-error tells them what to fix. Composing a trimmed
    // value would hide the mistake.
    const form: SetupCredentialsForm = {
      technicalUserLogin: "  techuser  ",
      technicalUserPassword: "  pw  ",
      xmlSignKey: "  sk  ",
      xmlChangeKey: "  ck  ",
    };
    const body = composeSetupCredentialsBody(form);
    expect(body.technical_user_login).toBe("  techuser  ");
    expect(body.technical_user_password).toBe("  pw  ");
    expect(body.xml_sign_key).toBe("  sk  ");
    expect(body.xml_change_key).toBe("  ck  ");
  });
});

describe("validateSetupCredentials", () => {
  it("accepts a fully-populated form", () => {
    const result = validateSetupCredentials(fullForm());
    expect(result.ok).toBe(true);
    expect(result.technicalUserLogin).toBeNull();
    expect(result.technicalUserPassword).toBeNull();
    expect(result.xmlSignKey).toBeNull();
    expect(result.xmlChangeKey).toBeNull();
  });

  it("rejects an empty technical-user login", () => {
    const form = { ...fullForm(), technicalUserLogin: "" };
    const result = validateSetupCredentials(form);
    expect(result.ok).toBe(false);
    expect(result.technicalUserLogin).not.toBeNull();
  });

  it("rejects a whitespace-only technical-user login", () => {
    // The login is NOT a secret; whitespace in it is meaningless.
    // Trim-then-non-empty mirrors the backend validator.
    const form = { ...fullForm(), technicalUserLogin: "   " };
    const result = validateSetupCredentials(form);
    expect(result.ok).toBe(false);
    expect(result.technicalUserLogin).not.toBeNull();
  });

  it("rejects an empty password", () => {
    const form = { ...fullForm(), technicalUserPassword: "" };
    const result = validateSetupCredentials(form);
    expect(result.ok).toBe(false);
    expect(result.technicalUserPassword).not.toBeNull();
  });

  it("rejects an empty XML sign key", () => {
    const form = { ...fullForm(), xmlSignKey: "" };
    const result = validateSetupCredentials(form);
    expect(result.ok).toBe(false);
    expect(result.xmlSignKey).not.toBeNull();
  });

  it("rejects an empty XML change key", () => {
    const form = { ...fullForm(), xmlChangeKey: "" };
    const result = validateSetupCredentials(form);
    expect(result.ok).toBe(false);
    expect(result.xmlChangeKey).not.toBeNull();
  });

  it("flags every missing field independently — no early-return", () => {
    // CLAUDE.md rule 9: per-field independent reporting so the
    // operator sees ALL the work needed in one pass rather than
    // playing whack-a-mole one field at a time. A regression that
    // converted to early-return would fail this pin.
    const empty: SetupCredentialsForm = {
      technicalUserLogin: "",
      technicalUserPassword: "",
      xmlSignKey: "",
      xmlChangeKey: "",
    };
    const result = validateSetupCredentials(empty);
    expect(result.ok).toBe(false);
    expect(result.technicalUserLogin).not.toBeNull();
    expect(result.technicalUserPassword).not.toBeNull();
    expect(result.xmlSignKey).not.toBeNull();
    expect(result.xmlChangeKey).not.toBeNull();
  });
});
