import { describe, expect, it } from "vitest";
// Vite's `?raw` — the component sources as strings.
import outgoing from "../routes/InvoiceList.svelte?raw";
import incoming from "../routes/IncomingInvoiceList.svelte?raw";

// ─────────────────────────────────────────────────────────────────────
// DELEGATION guard — deliberately the only thing left in this file.
//
// The behaviour of the three drill-down predicates is pinned by
// EXECUTING them in `aging-facets.test.ts`. Source-text assertions cannot
// do that job: this suite used to grep the components for a
// `payment_deadline === null` early-out, which a verdict flip
// (`return false` → `return true`) leaves untouched — green suite, wrong
// list.
//
// What source text CAN still pin is that the components have not
// reinstated a private copy of the classification. If a component starts
// calling `agingBucketFor` itself again, the behaviour pins keep passing
// (they test the shared module) while the shipped list quietly diverges
// from it. That is the one gap execution-based pins leave, and it is all
// this file covers now.
// ─────────────────────────────────────────────────────────────────────

const COMPONENTS: ReadonlyArray<readonly [string, string, readonly string[]]> = [
  ["InvoiceList.svelte", outgoing, ["outgoingAgingMatches"]],
  [
    "IncomingInvoiceList.svelte",
    incoming,
    ["incomingAgingMatches", "incomingPastDeadlineMatches"],
  ],
];

describe("invoice lists delegate their drill-down predicates", () => {
  for (const [name, source, delegates] of COMPONENTS) {
    for (const fn of delegates) {
      it(`${name} calls ${fn} from the shared module`, () => {
        expect(source).toContain(`${fn}(`);
        expect(source).toMatch(new RegExp(`import\\s*\\{[^}]*${fn}[^}]*\\}\\s*from\\s*"\\.\\./lib/aging-facets"`, "s"));
      });
    }

    it(`${name} does NOT classify deadlines itself`, () => {
      // A local `agingBucketFor` call is the private-copy tell: the
      // behaviour pins would still pass while this component drifted.
      expect(source).not.toContain("agingBucketFor(");
    });

    it(`${name} does NOT re-add its own null-deadline branch`, () => {
      // Not a behaviour assertion — the shared predicate already owns
      // that. This catches a component re-deriving the rule alongside
      // the delegate call, which is how the two get to disagree.
      expect(source).not.toMatch(/payment_deadline\s*(===?|!==?)\s*null/);
    });
  }
});
