<!--
Keep the description self-contained: a reader with only this repository in
front of them must be able to understand what changed and why. State the
rationale inline rather than pointing at external trackers or private notes.
-->

## What and why



## Subscription-CLI posture

This project drives subscription-billed interactive CLIs, and its standing posture is that sessions stay real, human-visible, interactive sessions and approvals always surface to a human — never silent auto-approval.

- [ ] This change does **not** enable detached, unattended, or multi-user use of a subscription-billed CLI, and does not add any path that resolves an approval without a human decision.

If the change touches that surface at all — even legitimately — explain here how the posture is kept:

## Scope

- **Milestone / goal this advances**:
- **Known risk or open question this touches** (state it here, or "none"):

## Checks

- [ ] `cargo xtask ci` is green locally (it is the PR tier, less the supply-chain gate below).
- [ ] If this changes dependencies: `cargo xtask deny` is green too. It is the one PR-tier check `cargo xtask ci` leaves out, because it needs `cargo-deny` installed.
- [ ] Any change to a committed golden trace or to `schema/` is called out above as a behavior change — traces and schema artifacts are contracts, not test fixtures.
