// PR-58 / session-78 — pin tests for `parseNavUpstreamFault`. The
// backend's HTTP-502 `nav_upstream_fault` body is the operator's
// actionable diagnostic when NAV's `tokenExchange` rejects at the HTTP
// layer (signature drift, IP whitelist mismatch, expired technical-
// user password). The Tauri forwarder stringifies non-2xx responses
// as `"backend returned <status> for <path>: <body>"`; this parser
// pulls the trailing JSON tail out and returns the typed shape only
// when the discriminator matches.
//
// Regression posture per CLAUDE.md rule 9: a parser that always
// returns `null` would silently drop the diagnostic and the SPA would
// fall back to the opaque string — exactly the failure mode this PR
// closes. The pins assert positive extraction, negative discriminator
// rejection, and graceful handling of non-JSON / malformed input.

import { describe, expect, it } from "vitest";

import { parseNavUpstreamFault } from "./api";

describe("parseNavUpstreamFault", () => {
  it("extracts a typed fault from the forwarder's wrapper string", () => {
    // Shape mirrors what `commands.rs::forward_post` produces on a
    // non-2xx response: a status prefix concatenated with the verbatim
    // JSON body the backend returned.
    const wrapped =
      'backend returned 502 Bad Gateway for /invoices/01HX.../submit: ' +
      '{"error":"nav_upstream_fault","status":400,' +
      '"fault_code":"INVALID_REQUEST_SIGNATURE",' +
      '"fault_message":"A digitális aláírás érvénytelen.",' +
      '"raw_body_preview":"<GeneralErrorResponse>...</GeneralErrorResponse>"}';
    const fault = parseNavUpstreamFault(wrapped);
    expect(fault).not.toBeNull();
    expect(fault?.status).toBe(400);
    expect(fault?.fault_code).toBe("INVALID_REQUEST_SIGNATURE");
    expect(fault?.fault_message).toBe("A digitális aláírás érvénytelen.");
    expect(fault?.raw_body_preview).toContain("GeneralErrorResponse");
  });

  it("rejects a JSON body whose discriminator is not nav_upstream_fault", () => {
    const wrapped =
      'backend returned 409 Conflict for /invoices/01HX.../submit: ' +
      '{"error":"POST /invoices/.../submit requires state `Ready`"}';
    expect(parseNavUpstreamFault(wrapped)).toBeNull();
  });

  it("returns null when the message has no JSON tail", () => {
    expect(parseNavUpstreamFault("network error: connection refused")).toBeNull();
  });

  it("returns null on malformed JSON (does not throw)", () => {
    const wrapped = "backend returned 502 for /x: {not actually json";
    expect(parseNavUpstreamFault(wrapped)).toBeNull();
  });

  it("preserves null fields when NAV's body parsing did not yield a typed pair", () => {
    // Backend returned the wrapper but could not parse `fault_code` /
    // `fault_message` (NAV returned an HTML error page, for example);
    // the typed pair lands as `null` and the SPA's render path falls
    // back to the body preview verbatim.
    const wrapped =
      'backend returned 502 Bad Gateway for /invoices/01HX.../submit: ' +
      '{"error":"nav_upstream_fault","status":500,' +
      '"fault_code":null,"fault_message":null,' +
      '"technical_validations":[],' +
      '"raw_body_preview":"<html><body>NAV maintenance</body></html>"}';
    const fault = parseNavUpstreamFault(wrapped);
    expect(fault).not.toBeNull();
    expect(fault?.fault_code).toBeNull();
    expect(fault?.fault_message).toBeNull();
    expect(fault?.technical_validations).toEqual([]);
    expect(fault?.raw_body_preview).toContain("NAV maintenance");
  });

  it("extracts the technical_validations array with per-rule fields", () => {
    // PR-59 / session-79 — NAV's `INVALID_REQUEST` top-level wrapper is
    // generic; the actual per-rule diagnostic NAV emits for a 400 lives
    // inside the repeated `<technicalValidationMessages>` array. The
    // backend parses and forwards the list as a flat JSON array; the
    // SPA renders each as a row in the fault panel. Regression posture:
    // a parser that returns this array empty silently drops the actual
    // reject reason — exactly the bug PR-59 closes.
    const wrapped =
      'backend returned 502 Bad Gateway for /invoices/01HX.../submit: ' +
      '{"error":"nav_upstream_fault","status":400,' +
      '"fault_code":"INVALID_REQUEST","fault_message":"Helytelen kérés!",' +
      '"technical_validations":[' +
      '{"result_code":"ERROR","error_code":"SCHEMA_VIOLATION",' +
      '"message":"Hiányzó kötelező mező: invoiceNumber",' +
      '"tag":"InvoiceData/invoiceNumber"},' +
      '{"result_code":"WARN","error_code":"CUSTOMER_TAX_NUMBER",' +
      '"message":"A vevő adószám ellenőrzése nem sikerült.",' +
      '"tag":"invoiceMain/invoice/invoiceHead/customerInfo/customerTaxNumber"}' +
      '],"raw_body_preview":"<GeneralErrorResponse>...</GeneralErrorResponse>"}';
    const fault = parseNavUpstreamFault(wrapped);
    expect(fault).not.toBeNull();
    expect(fault?.fault_code).toBe("INVALID_REQUEST");
    expect(fault?.technical_validations).toHaveLength(2);
    expect(fault?.technical_validations[0].result_code).toBe("ERROR");
    expect(fault?.technical_validations[0].error_code).toBe("SCHEMA_VIOLATION");
    expect(fault?.technical_validations[0].message).toBe(
      "Hiányzó kötelező mező: invoiceNumber",
    );
    expect(fault?.technical_validations[0].tag).toBe(
      "InvoiceData/invoiceNumber",
    );
    expect(fault?.technical_validations[1].result_code).toBe("WARN");
    expect(fault?.technical_validations[1].error_code).toBe(
      "CUSTOMER_TAX_NUMBER",
    );
  });

  it("normalises a missing technical_validations field to an empty array", () => {
    // Backwards-compatibility — a pre-PR-59 backend (or a schema
    // regression that drops the field) MUST still parse cleanly so the
    // SPA renderer can iterate without a null check. Loud regression
    // pin: if the parser leaves the field `undefined`, the renderer's
    // `{#each}` block crashes.
    const wrapped =
      'backend returned 502 Bad Gateway for /invoices/01HX.../submit: ' +
      '{"error":"nav_upstream_fault","status":400,' +
      '"fault_code":"INVALID_REQUEST_SIGNATURE",' +
      '"fault_message":"signature mismatch",' +
      '"raw_body_preview":"<x/>"}';
    const fault = parseNavUpstreamFault(wrapped);
    expect(fault).not.toBeNull();
    expect(fault?.technical_validations).toEqual([]);
  });
});
