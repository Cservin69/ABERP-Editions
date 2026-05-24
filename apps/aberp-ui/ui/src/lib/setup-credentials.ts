// PR-46α / session-62 — form-to-request composer for the first-run
// NAV-credentials setup wizard. Mirrors the A156 / A161 / A163
// composer-pin pattern: the operator-facing form state is one shape;
// the backend's `POST /api/setup-nav-credentials` body is another;
// this module owns the conversion and is the load-bearing test
// surface for the SPA-side wire shape.
//
// Splitting the composer out of the Svelte component (rather than
// inlining the JSON-build inside an event handler) keeps the
// component-test runner named-deferred per CLAUDE.md rule 2 — vitest
// pins the composer without mounting the wizard.

import type { SetupNavCredentialsRequest } from "./api";

/** Operator-facing form state for the wizard. Field names match the
 * form labels (camelCase per the rest of the SPA); the composer
 * snake-cases them on the way to the backend wire shape. */
export interface SetupCredentialsForm {
  technicalUserLogin: string;
  technicalUserPassword: string;
  xmlSignKey: string;
  xmlChangeKey: string;
}

/** Validation result for the form. `null` errors mean the field is
 * acceptable; a string is the operator-facing inline-error message. */
export interface SetupCredentialsValidation {
  technicalUserLogin: string | null;
  technicalUserPassword: string | null;
  xmlSignKey: string | null;
  xmlChangeKey: string | null;
  /** `true` iff every per-field error is `null`. */
  ok: boolean;
}

/** Per-field validator. Each field must be non-empty after trim —
 * mirrors the backend's validator in
 * `setup_nav_credentials::setup_credentials_from_inputs`. Surfacing
 * the validation client-side too keeps the operator from POSTing a
 * known-bad request (the backend would have rejected it with 400,
 * but the round-trip is wasted). */
export function validateSetupCredentials(
  form: SetupCredentialsForm,
): SetupCredentialsValidation {
  const technicalUserLogin =
    form.technicalUserLogin.trim().length === 0
      ? "Technical-user login is required"
      : null;
  const technicalUserPassword =
    form.technicalUserPassword.length === 0
      ? "Technical-user password is required"
      : null;
  const xmlSignKey =
    form.xmlSignKey.length === 0 ? "XML sign key is required" : null;
  const xmlChangeKey =
    form.xmlChangeKey.length === 0 ? "XML change key is required" : null;
  const ok =
    technicalUserLogin === null &&
    technicalUserPassword === null &&
    xmlSignKey === null &&
    xmlChangeKey === null;
  return {
    technicalUserLogin,
    technicalUserPassword,
    xmlSignKey,
    xmlChangeKey,
    ok,
  };
}

/** Compose the wire request body from the form state. Mirror of the
 * Rust-side `serve::SetupNavCredentialsRequest` shape (snake_case
 * field names). Pre-condition: `validateSetupCredentials(form).ok`
 * is `true` — the caller is expected to gate the submit button on
 * the validator, but the composer itself does NOT re-validate (the
 * backend is the final gate per A157). */
export function composeSetupCredentialsBody(
  form: SetupCredentialsForm,
): SetupNavCredentialsRequest {
  return {
    technical_user_login: form.technicalUserLogin,
    technical_user_password: form.technicalUserPassword,
    xml_sign_key: form.xmlSignKey,
    xml_change_key: form.xmlChangeKey,
  };
}
