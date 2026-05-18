// Pure-helper compile-check for the strict port parser used by
// the remote SSH create form. `Number.parseInt` would otherwise
// happily accept partial strings like "22abc" (returning 22) or
// "1.5" (returning 1), letting a typo silently probe / register
// a different endpoint than the user typed.
//
// Runs under tsx; no DOM / no React imports. Exits non-zero on
// any assertion failure.

import { parseStrictPort } from "../src/parse-strict-port";

function fail(label: string, why: string): never {
  console.error(`FAIL ${label}: ${why}`);
  process.exit(1);
}

function expectOk(label: string, raw: string, expectedPort: number): void {
  const r = parseStrictPort(raw);
  if (!r.ok) fail(label, `expected ok=true; got ${JSON.stringify(r)}`);
  if (r.port !== expectedPort)
    fail(label, `expected port=${expectedPort}; got ${r.port}`);
  console.log(`PASS ${label}`);
}

function expectFail(label: string, raw: string): void {
  const r = parseStrictPort(raw);
  if (r.ok) fail(label, `expected ok=false; got ${JSON.stringify(r)}`);
  console.log(`PASS ${label}`);
}

// Happy path: in-range integer.
expectOk("'22' parses as 22", "22", 22);
expectOk("'1' is the minimum allowed", "1", 1);
expectOk("'65535' is the maximum allowed", "65535", 65535);
expectOk("'2222' parses as 2222", "2222", 2222);

// Trim before validate.
expectOk("'  22  ' trims then accepts", "  22  ", 22);

// Reject empty + whitespace-only.
expectFail("empty string rejected", "");
expectFail("whitespace-only rejected", "   ");
expectFail("tab + space rejected", "\t  \n");

// Reject partial decimals — the original bug.
expectFail("'22abc' rejected (parseInt would accept as 22)", "22abc");
expectFail("'abc22' rejected (parseInt would NaN)", "abc22");
expectFail("'1.5' rejected (parseInt would accept as 1)", "1.5");
expectFail("'22 ' middle-space rejected after trim leaves '22'", "2 2");
expectFail("'0x16' hex rejected (parseInt would accept as 0)", "0x16");
expectFail("'1e3' scientific rejected (parseInt would accept as 1)", "1e3");

// Reject out-of-range.
expectFail("'0' below minimum rejected", "0");
expectFail("'65536' above maximum rejected", "65536");
expectFail("'70000' above maximum rejected", "70000");

// Reject explicit negatives.
expectFail("'-1' rejected (the regex blocks the leading minus)", "-1");
expectFail("'-22' rejected", "-22");

// Reject obviously absurd values.
expectFail("'1234567890' well above max rejected", "1234567890");

console.log("parse-strict-port: all assertions passed");
