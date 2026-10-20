<img width="2000" height="705" alt="sylox-lockup-horizontal-mono-oatmeal" src="https://github.com/user-attachments/assets/f942d26b-1746-4ed5-8658-9e4db0398595" />


# Sylox Protocol

**A public risk score for every anchor-issued token on Stellar, and a market where holders can buy protection against that issuer depegging, freezing funds, halting withdrawals, or failing.**

[Website](https://sylox.xyz) · [App (testnet)](https://app.sylox.xyz) · [Docs](https://docs.sylox.xyz/docs) · [Technical spec](./technical-doc.md) · [PRD](./prd.md)

> **Testnet only.** Sylox is under active development and runs on Stellar testnet. Nothing here holds real value, and every score shown by the testnet deployment is a demonstration, not a real risk assessment.

---

## Table of contents

- [The problem](#the-problem)
- [What Sylox does](#what-sylox-does)
- [How it works](#how-it-works)
  - [1. Anchor Risk Oracle](#1-anchor-risk-oracle)
  - [2. Credit Event Registry](#2-credit-event-registry)
  - [3. Protection Markets](#3-protection-markets)
- [A worked example](#a-worked-example)
- [Who it is for](#who-it-is-for)
- [Who is trusted, and how that trust is bounded](#who-is-trusted-and-how-that-trust-is-bounded)
- [Architecture](#architecture)
- [Project status](#project-status)
- [Repository layout](#repository-layout)
- [Getting started](#getting-started)
- [Testnet deployment](#testnet-deployment)
- [Documentation map](#documentation-map)
- [License](#license)

---

## The problem

On Stellar, a "dollar" or "peso" token is only as good as the company that issued it.

Stellar's anchor model lets many companies issue tokens for the same currency. Two tokens both called `USD` or `ARS` can carry very different risk: different reserves, jurisdictions, banking partners and operational track records. Wallets usually list them side by side with no risk signal at all.

Those issuers can fail in several distinct ways:

| Failure | What it looks like |
| --- | --- |
| **Depeg** | The token trades well below face value because people doubt they can redeem it. |
| **Issuer freeze** | The issuer revokes authorization or claws back balances (Stellar asset flags allow this). |
| **Withdrawal halt** | The anchor stops paying out fiat through its SEP-24 / SEP-6 flows, often before any price moves. |
| **Insolvency** | The issuer loses its banking partner, its licence, or its reserves. |

Today nobody holding these tokens can answer two basic questions in a shared, verifiable way:

1. **How risky is this issuer right now?**
2. **How do I protect myself if it fails?**

Individuals and remitters can only guess. Wallets do manual due diligence. Lending protocols set conservative limits blindly. NGOs and treasuries spread holdings by hand. And good anchors have no way to prove onchain that they are safer than their rivals.

## What Sylox does

Sylox answers both questions with a protocol, not an app. Its rules live in Soroban smart contracts, so anyone can read the risk feed, and any wallet, lender or contract can build on it.

It has three parts that feed each other:

```
  Ledger data + anchor endpoints
              │
              ▼
 ┌──────────────────────────┐     "How risky is this issuer right now?"
 │ 1. Anchor Risk Oracle    │ ──► public score (0-100) and band per asset
 └──────────────────────────┘
              │ signal history
              ▼
 ┌──────────────────────────┐     "Has this issuer actually failed?"
 │ 2. Credit Event Registry │ ──► objective, challengeable declaration
 └──────────────────────────┘
              │ Declared
              ▼
 ┌──────────────────────────┐     "Pay me if it does."
 │ 3. Protection Markets    │ ──► fully collateralized cover, paid in USDC
 └──────────────────────────┘
```

- **A risk feed** that turns hidden issuer risk into public, recomputable onchain data.
- **A credit event standard** with fixed, versioned definitions of what "failure" means and an onchain process for declaring it.
- **Protection markets** where buyers pay a premium for cover and sellers lock USDC to back it. If a covered event is declared, every unit of cover pays exactly 1 USDC.

What Sylox is **not**:

- **Not a credit rating agency.** Scores come from an open formula over public data, not from opinion.
- **Not leveraged.** Every unit of cover is backed one to one by collateral posted up front.
- **Not a promise of returns.** Sellers earn premiums for taking on real risk and can lose their collateral.

## How it works

### 1. Anchor Risk Oracle

The `RiskOracle` contract keeps a rolling history of signals for each tracked asset and computes a risk score onchain.

**Where the signals come from**

| Signal | Source |
| --- | --- |
| Peg ratio | Trades on the classic DEX and AMM pools, volume weighted, against the asset's reference (USD, or a fiat FX rate) |
| Liquidity within 2% of peg | Order book offers and pool reserves near the peg |
| Redemption flow | Payments to and from the issuer account, and burns |
| Supply and supply change | Ledger asset stats |
| Issuer actions | Clawbacks, trustline flag changes and set-options operations by the issuer |
| Endpoint health | Independent, staked **reporters** probing each anchor's `stellar.toml` and transfer server from different regions |

**How signals get onchain.** Soroban contracts cannot read the classic DEX or most classic account state directly, so bonded **keepers** compute the signals offchain from public ledger data and post them along with a hash of their raw inputs. Each posting sits in a dispute window first: anyone can rerun the open-source recomputation tool on the same public data and, if they get a different answer, dispute the posting by locking a bond. A keeper caught posting bad data is slashed. Where an onchain source exists (for example a Soroban AMM pair), the contract cross checks the keeper's price against it directly.

**The score.** Six components, each 0 to 100, are combined with governance-set weights:

| Component | Weight | Measures |
| --- | --- | --- |
| Peg deviation (P) | 35% | How far the price sits from peg (10th percentile, so one wick cannot move it) |
| Endpoint health (E) | 20% | Whether the anchor's withdrawal endpoints are up, degraded or down |
| Redemption pressure (R) | 15% | Net outflow relative to supply |
| Issuer actions (I) | 15% | Clawbacks and authorization revocations |
| Liquidity (L) | 10% | Depth near peg relative to a target |
| Supply shock (S) | 5% | Sudden supply changes |

The score maps to a **band** that wallets and lenders can act on:

| Band | Score | Meaning |
| --- | --- | --- |
| Normal | 0 to 24 | Nothing unusual |
| Watch | 25 to 49 | Worth keeping an eye on |
| Warning | 50 to 74 | Material stress (forced here if the peg or endpoints max out) |
| Distress | 75 to 100 | Severe stress, or a credit event is in progress |
| Event | n/a | A credit event has been declared |

Bands move up immediately but only move down after several consecutive calm hours, so they do not flap. If data stops arriving, the asset is marked **stale** rather than silently showing an old score.

### 2. Credit Event Registry

The `EventRegistry` contract is the only thing that can declare an issuer failed, and a declaration is the only thing that releases payouts. Every asset has one canonical, versioned definition per failure kind, fixed in advance (for example: "every hourly price in a 72 hour window below 0.95 of peg").

Events come in three tiers, depending on how objective the evidence is:

| Tier | Kinds | Who can propose | Evidence |
| --- | --- | --- | --- |
| **Tier 1**: data verified | Depeg, IssuerFreeze, MintWithoutBacking (not built yet) | Anyone, no bond needed | The oracle's finalized, recomputable signal history |
| **Tier 2**: reporter claim | WithdrawalHalt | Anyone, with a bond | Reporter probes, stuck SEP-24 transactions, anchor cooperation |
| **Tier 3**: committee | Insolvency | The committee | Published written reasoning |

Every event follows the same lifecycle: **Proposed → (Challenged → Escalated) → Declared or Rejected**. Anyone can challenge a proposal within its challenge window by posting a bond, which escalates it to a 4 of 7 committee. The committee must rule before a fixed deadline; if it does not, a predetermined default outcome applies, so no event can be stalled forever. A Depeg can also **cure** if the price stays above the cure threshold (0.98 by default) for the whole challenge window.

While any event is in progress, the asset's band is forced to at least Distress and new cover sales stop.

### 3. Protection Markets

Each protection market is a **series**: one asset, one term, one pinned set of event definitions, deployed as its own contract so a problem in one series can never touch another's funds.

- **Sellers** deposit USDC collateral and post a quote (an annualized premium rate). Their collateral backs the cover they write, one to one.
- **Buyers** fill quotes cheapest first and receive fungible **cover units** (a standard SEP-41 token, so wallets can show and transfer them). 1 cover unit pays 1 USDC if a covered event is declared.
- **Premiums** are priced on the time remaining until expiry. A protocol fee (default 7.5%) goes to the treasury; the rest goes to the seller.
- **If an event is declared**, anyone can trigger the series and holders claim 1 USDC per cover unit. Sellers withdraw whatever collateral was not used, plus their premiums.
- **If no event happens**, the series expires, sellers withdraw all their collateral plus premiums, and cover units are worth nothing.

Several protections are built in so the market cannot be gamed:

- **No informed buying.** Cover cannot be bought while an event is in progress, while recent data already shows failure signals, or while the asset is stale or distressed.
- **Coverage is keyed to when the failure started**, not when it was proposed, so a depeg that begins near expiry is still paid out.
- **Manipulation is unprofitable by design.** Total open cover on an asset is capped at a fraction of its measured liquidity, so the cost of faking a depeg exceeds what the cover would pay.
- **Sellers cannot take premiums and run.** Premiums are only withdrawable once the outcome is known.

## A worked example

1. Seller A deposits 100,000 USDC and quotes 4% a year. Seller B deposits 50,000 USDC and quotes 3% a year.
2. A treasury holding a USD token buys 80,000 USDC of cover with 90 days left. It fills 50,000 from B (cheapest) and 30,000 from A, paying about 666 USDC in total premium. About 50 USDC of that is the protocol fee.
3. **If nothing happens:** at expiry A withdraws 100,000 plus about 274 USDC, and B withdraws 50,000 plus about 342 USDC.
4. **If the issuer depegs and the event is declared:** the buyer claims 80,000 USDC. A withdraws the 70,000 that was never used plus premium; B's 50,000 has been paid out and B withdraws only its premium.

The full arithmetic, with rounding rules, is in [`technical-doc.md` Section 10.5](./technical-doc.md#105-worked-example).

## Who it is for

| You are | Sylox gives you |
| --- | --- |
| **A wallet** | A risk badge for every issued asset in a user's balance, readable for free via RPC simulation |
| **A lending protocol** | Bands to plug into collateral factors and borrow limits, read directly inside your own contract |
| **A treasury, NGO or fintech** | Cover against the issuer of the tokens you hold failing |
| **A market maker or treasury with idle USDC** | Premium income for underwriting issuer risk you understand |
| **An anchor** | A public, verifiable record that you are running safely |
| **A reporter or keeper operator** | Rewards for running the probes and posting the data the feed depends on |

Integration recipes for each are in [`technical-doc.md` Section 20](./technical-doc.md#20-integration-guides) and on the [docs site](https://docs.sylox.xyz/docs/integration-guides).

## Who is trusted, and how that trust is bounded

Sylox does not pretend to be trustless in v1. The trust root for payouts is a set of **permissioned, bonded keepers** plus a **4 of 7 committee**. That trust is bounded, not hidden:

- **Every signal is recomputable.** Raw inputs are published and the recomputation tool is open source. Anyone can dispute a posting.
- **Every role has money at stake.** Keepers post bonds, reporters post stakes, and disputers and challengers post bonds. Losers are slashed.
- **Every step is permissionless to trigger.** Finalizing, triggering, claiming and resolving timeouts need no special role.
- **Every committee decision has a deadline** with a fixed default outcome, and missed deadlines are recorded onchain.
- **Governance cannot touch open series.** Parameter changes go through a timelock and never apply retroactively. A guardian can pause new activity but can never pause claims or withdrawals.

The path to decentralizing further (open keeper and reporter registration, onchain verifiable disputes, an onchain recusal list for the committee) is described in [`technical-doc.md` Section 1.6](./technical-doc.md#16-trust-model-in-v1).

## Architecture

Sylox v1 is seven Soroban contracts plus offchain services and clients. Money only ever sits in three places:

| Contract | Role | Holds funds? |
| --- | --- | --- |
| `RiskOracle` | Signals, ring buffer of history, score, bands, staleness, signal disputes | No |
| `EventRegistry` | Canonical event definitions, proposals, challenges, rulings, event status | No |
| `Staking` | Keeper bonds, reporter stakes and probes, bond escrow, slashing | **Yes**: bonds and stakes |
| `Treasury` | Protocol fees, slashed funds, keeper and reporter reward pools | **Yes**: fees, slashed funds, rewards |
| `MarketFactory` | Opens series, enforces asset-wide cover caps atomically | No |
| `Series` | One per market: collateral, quotes, cover tokens, premiums, claims | **Yes**: collateral and premiums |
| `Governor` | Multisig, timelocked parameter changes, upgrades, pausing | No |

Data flows one way: reporters and keepers feed the oracle, the oracle feeds the registry, and the registry's status decides how each series settles. Series pull event status when someone calls them, so the registry never needs to know how many series exist.

Offchain, a **keeper** computes and posts signals, a **reporter node** probes anchor endpoints, an **indexer** turns contract events into queryable history, and a read-only **public API** serves apps. None of them is trusted for payouts: everything they post is checked or disputable onchain.

For the full picture, including architecture, state machine and call sequence diagrams, see [`technical-doc.md` Section 2](./technical-doc.md#2-system-architecture) onwards.

## Project status

| Component | Status |
| --- | --- |
| Shared types (`contracts/types`) | Implemented, with encoding tests |
| `RiskOracle` | Implemented and tested; deployed to testnet |
| `Staking` | Implemented and tested; deployed to testnet |
| `Treasury` | Implemented and tested; deployed to testnet |
| `EventRegistry` | Implemented and tested (Depeg and IssuerFreeze, Tier 1); deployed to testnet |
| `MarketFactory`, `Series` | Implemented and tested; not yet part of the testnet deployment |
| `Governor` | Implemented and tested; not yet deployed. The testnet admin key stands in for it until the handover in [`docs/design/governor.md`](./docs/design/governor.md) Section 10 |
| AMM price and FX adapters | Stubs |
| `@sylox/sdk` (`packages/sdk`) | In use by the web app |
| Web app (`web/app`) | Live on testnet: asset explorer, asset pages, events list, event page with Checkpoint and Finalize actions. Markets, positions and faucet screens are not built yet |
| Landing site and docs site | Live at [sylox.xyz](https://sylox.xyz) and [docs.sylox.xyz](https://docs.sylox.xyz/docs) |
| Keeper, reporter node, indexer, API services | Not built yet; testnet uses a scripted demo poster in their place |
| WithdrawalHalt (Tier 2), Insolvency (Tier 3) | Later build phase |

A real IssuerFreeze event has already been run end to end on testnet against a sample `DEMOUSD` asset; see [`docs/demousd-issuer-freeze-2026-10-11.md`](./docs/demousd-issuer-freeze-2026-10-11.md).

## Repository layout

```
contracts/                 Soroban smart contracts (Rust, one Cargo workspace)
  types/                   shared #[contracttype]s imported by every contract
  risk-oracle/             signals, score, bands (technical-doc.md Sections 5-6)
  event-registry/          credit event state machine (Section 8)
  staking/                 keeper bonds, reporter stakes and probes, bond escrow, slashing (Section 7)
  treasury/                protocol fees, slashed funds, reward pools (Section 12.7)
  market-factory/          opens series, enforces global caps (Sections 9, 11.1)
  series/                  one protection market per series (Sections 9-10)
  governor/                multisig, timelock, upgrades, pausing (Section 17)
  adapters/                AMM price and FX reference adapters (Section 3.3)
  price-guard-mock/        test double for the optional PriceGuard
services/                  offchain services (placeholders, Section 18)
  keeper/  reporter-node/  indexer/  api/
packages/
  sdk/                     @sylox/sdk, TypeScript client over the generated bindings
  recompute/               deterministic signal recomputation tool
web/                       npm workspace for the JS/TS side
  app/                     the Sylox app (Next.js), talks to contracts via @sylox/sdk
  landing/                 marketing site (Next.js, static export)
  docs/                    documentation site (Fumadocs)
  shared/ui/               design system shared by app and landing
deploy/                    testnet deploy, demo signal posting, smoke tests
deployments/               live contract ids and Wasm hashes per network
docs/                      design notes, ADRs, event write-ups
tests/                     integration, scenario and property test suites
```

## Getting started

### Contracts

Requires Rust with the `wasm32v1-none` target and the [Stellar CLI](https://developers.stellar.org/docs/tools/cli).

```sh
cargo check --workspace
cargo test --workspace
stellar contract build
```

### Web app, landing site, docs and SDK

Everything JS/TS is an npm workspace. Install and run from the repo root:

```sh
npm install
npm run build -w packages/sdk   # the app and docs use the SDK's built output
npm run dev -w web/app        # the Sylox app
npm run dev -w web/landing    # the marketing site
npm run dev -w web/docs       # the docs site
```

See [`web/README.md`](./web/README.md) and [`web/app/README.md`](./web/app/README.md) for details.

## Testnet deployment

A live deployment of the four contracts that were first to merge (`RiskOracle`, `Staking`, `Treasury`, `EventRegistry`) runs on Stellar testnet, wired to each other, with one real asset registered; its score appears once 7 days of history exist. Two testnet-only stand-ins are used, and both are replaced as the real contracts land:

- **The admin key stands in for Governor.** A dedicated, funded testnet identity (`sylox-testnet-admin`) is passed wherever a contract expects a `governor` address. It holds no real value.
- **TUSD is test-only collateral, not real money.** An active keeper needs a bond the Circle testnet USDC faucet can't supply at the needed scale, so testnet uses a self-issued asset, "TUSD". It is never called USDC anywhere in this deployment; the asset Sylox actually tracks and scores is separate, real Circle testnet USDC.
- **Demo signals are synthetic.** Until a keeper service exists (issues [#20](https://github.com/Sylox-labs/sylox/issues/20) and [#24](https://github.com/Sylox-labs/sylox/issues/24)), `deploy/post-demo-signals.sh` posts invented, plausible-looking data. Treat every score and band from this deployment as a demonstration.

Requires the `stellar` CLI version pinned in `.github/workflows/ci.yml` (`STELLAR_CLI_VERSION`) and `jq`. Run in this order (see [`deploy/README.md`](./deploy/README.md) for why the order matters):

```sh
# 1. Deploy all four contracts, issue TUSD, register the tracked asset,
#    write deployments/testnet.json. Every run deploys fresh contract instances
#    and rewrites deployments/testnet.json, so on the shared testnet
#    deployment only run it as part of a planned redeploy.
deploy/deploy.sh testnet

# 2. Post synthetic demo signals for the last 24 closed epochs (or pass a
#    different count), so RiskOracle has something to score.
deploy/post-demo-signals.sh testnet [epochs]

# 3. Verify the deployment: every contract responds, the keeper is active,
#    the reward bucket is funded, the tracked asset is registered, a fresh
#    signal posts and reads back, and a non-admin call to a governor-gated
#    function is rejected.
deploy/smoke-testnet.sh
```

Current contract ids, Wasm hashes and identity public keys are in [`deployments/testnet.json`](./deployments/testnet.json), rewritten on every `deploy.sh testnet` run. No secret key ever appears in this repository; every identity's secret lives only in the local `stellar keys` store.

Known gaps specific to this deployment: committee-gated functions (`rule`, `resolve_timeout`, signal dispute resolution) don't work until the Governor is deployed and the testnet contracts point at it ([#22](https://github.com/Sylox-labs/sylox/issues/22)); EventRegistry's `factory` argument is a placeholder until MarketFactory is deployed ([#23](https://github.com/Sylox-labs/sylox/issues/23)).

## Documentation map

| Document | What's in it |
| --- | --- |
| [`prd.md`](./prd.md) | Product requirements: the problem, users, protocol scope, go-to-market, SCF application plan |
| [`technical-doc.md`](./technical-doc.md) | The engineering specification: contracts, data model, score math, event state machine, markets, settlement math, APIs, events, errors, storage, roles, governance, services, SDK, security, deployment and parameters. Includes architecture, state and sequence diagrams. Starts with the changelog |
| [`docs/decisions/`](./docs/decisions/) | Architecture decision records ADR-001 to ADR-012 behind the spec |
| [`docs/design/`](./docs/design/) | Design notes for the event registry and markets |
| [`deploy/README.md`](./deploy/README.md) | Testnet deploy order and operational notes |
| [docs.sylox.xyz](https://docs.sylox.xyz/docs) | Concepts, protection markets, integration guides, SDK and contract reference |

**Where to start in `technical-doc.md`:**

| You are | Read first |
| --- | --- |
| Contract engineer | Sections 4 to 17, 21 |
| Offchain or infra engineer | Sections 5, 7, 18, 22 |
| Wallet or lender integrator | Sections 6, 12, 13, 19, 20 |
| Auditor | Sections 1.6, 7.8, 10, 11, 12.7, 15, 16, 21 |
| Reporter or committee member | Sections 1.6, 7, 8, 20 |

## License

Apache-2.0. See [`LICENSE`](./LICENSE).
