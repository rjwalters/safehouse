# Work Plan

Prioritized roadmap generated from current GitHub label state, maintained by
the Guide triage agent. Regenerated whenever label state changes.

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

_None._

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#94**: Host onboarding needs a human on the homeserver, which caps the fleet at the rate an operator can mint Matrix accounts — blocks dynamic scale-out to hundreds of hosts
- **#22**: Upgrade tuwunel to v1.8.3 for MSC4108 QR login (Element X onboarding)

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#22**: Upgrade tuwunel to v1.8.3 for MSC4108 QR login (Element X onboarding)

## In Progress

Issues currently being built (`loom:building`).

_None._

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#102**: feat: add scripted unencrypted claims-room creation (D6 carve-out)

## Proposed

Issues carrying `loom:curated`.

- **#94**: Host onboarding needs a human on the homeserver, which caps the fleet at the rate an operator can mint Matrix accounts — blocks dynamic scale-out to hundreds of hosts *(curated)*
- **#24**: Retire the Studio rollback backup once the EC2 homeserver is proven stable *(curated)*
- **#22**: Upgrade tuwunel to v1.8.3 for MSC4108 QR login (Element X onboarding) *(curated)*

## Proposed (Architect / Hermit)

_None._

## Epics

_None._

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 0 |
| Urgent | 2 |
| Ready (`loom:issue`) | 1 |
| In Progress (`loom:building`) | 0 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 1 |
| Curated | 3 |
| Architect / Hermit proposals | 0 |
| Active epics | 0 |
<!-- guide:plan-body:end -->
