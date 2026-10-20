PRD: Sylox Protocol
Oct 8, 2026 · @david
1. Document control and executive summary
Sylox is a Soroban protocol that measures the risk of each Stellar anchor and stablecoin issuer, and lets people buy and sell protection against that issuer failing. Target: SCF Build Award, Open Track, full protocol scope.
Field
Value
Project
Sylox Protocol (working name; check for collisions in Phase 0)
Status
Pre protocol / validation
Version
v1, October 8, 2026
SCF track
Build Award, Open Track
Target round
SCF #47 (the #46 deadline of Nov 8, 2026 is too close for this scope)
Scope
Full protocol: risk feed, protection market, offchain triggers, mainnet
Draft ask
$140K to $150K in XLM (Section 16)


1.1 One line
Sylox gives every anchor issued token on Stellar a public risk score and a market where holders can insure against that issuer depegging, freezing withdrawals or failing.
1.2 The protocol in three parts
1. Anchor Risk Oracle: a public onchain feed per issued asset, combining price, issuer account behaviour and anchor endpoint health into signals and a risk score.
2. Credit Event Registry: objective definitions of failure (depeg, issuer freeze, halted withdrawals, insolvency) and a process that declares them, using keeper posted, publicly recomputable data checks, staked reporters with disputes, and a committee for edge cases.
3. Protection Markets: fully collateralized pools per issued asset. Buyers pay premiums for cover; sellers post USDC and earn premiums; payouts settle automatically when a credit event is declared.
1.3 Why it matters
Stellar's model is many issuers for the same currency. Users, wallets, lenders and NGOs hold these tokens but have no shared way to see, price or hedge issuer risk. Sylox turns that hidden risk into public data and a tradable market, which also gives good anchors a way to prove they are safe.
1.4 The honest summary
The contracts are the easy part. The hard parts are reliable credit event detection, market manipulation on thin liquidity, getting both buyers and sellers, and legal structure, since protection like this is likely a derivative or insurance in most jurisdictions. This PRD treats those four as first class workstreams, not footnotes.
1.4a Who is trusted in v1
In v1 the trust root for payouts is a set of permissioned, bonded keepers plus a 4 of 7 committee. Every step is permissionless to trigger and every signal is recomputable, but keepers and reporters are permissioned and the committee resolves disputes.
The committee must rule within a fixed deadline, with a default outcome if it does not (Section 6.4). The path away from this trust root is in Section 17.2: open keeper and reporter registration with higher stakes, and onchain verifiable signal disputes, in later versions.
2. The problem
On Stellar, a "dollar" or "peso" token is only as good as its issuer, yet there is no public measure of issuer risk and no way to hedge it.
2.1 Many issuers, one name
Stellar's anchor model lets several companies issue tokens for the same currency. Two tokens both called USD or ARS can carry very different risk: different reserves, jurisdictions, banking partners and operational track records. Wallets usually show them side by side with no risk signal.
2.2 Failure modes are real and varied
• Depeg: the token trades well below its face value because people doubt redemption.
• Issuer action: the issuer freezes accounts, revokes authorization or claws back balances (Stellar supports these asset flags).
• Halted withdrawals: the anchor stops paying out fiat through its SEP-24 or SEP-6 flows, often before any price moves.
• Insolvency or enforcement: the issuer loses its banking partner, licence or reserves.
2.3 Who carries the risk today
Holder
Exposure
What they can do today
Individuals and remitters
Savings or salary in a local currency token
Nothing, except guess
Wallets
Choose which issuers to show and recommend
Manual due diligence, no shared data
Lending protocols
Accept issued tokens as collateral
Set conservative limits blindly
NGOs and treasuries
Hold operating funds in local tokens
Spread holdings by hand
Good anchors
Cannot prove they are safer than rivals
Publish reports nobody can verify onchain
2.4 The gap
There is no shared, onchain answer to two questions: how risky is this issuer right now, and how do I protect myself if it fails. Sylox answers both, and makes the answer composable for any wallet or protocol.
3. Protocol overview and positioning
Sylox is a protocol, not an app: its rules live in Soroban contracts, anyone can read the risk feed, open a market or buy cover, and other protocols can build on its signals and events.
3.1 Why it is a protocol
• Rules onchain: signals, credit event status, collateral, premiums and payouts are held and enforced by contracts.
• Permissionless use: any wallet or contract can read the feed; any user can buy or sell protection; anyone can propose a data verified event, or challenge any event or signal by posting a bond. Keepers and reporters are permissioned in v1 (Section 1.4a).
• Composable outputs: risk scores and credit event flags are onchain data that lenders, wallets and auto switch tools can act on.
• Survives the team: deployed contracts keep running and paying out without us.
3.2 What it is
• A public, onchain risk feed for Stellar issued assets.
• A credit event standard with an onchain determination process.
• Fully collateralized protection markets, one per issued asset.
• A reference web app and SDK for wallets and protocols (clients, not the protocol).
3.3 What it is not
• Not a credit rating agency. Scores are formula driven from public data, and the formula is open.
• Not leveraged. Every unit of cover is backed by posted collateral.
• Not an insurer of last resort. Payouts are limited to pool collateral.
• Not a stablecoin or a lender. It holds no reserves for anyone and issues no stable asset.
• Not an anchor auditor. It reads public signals; it does not inspect bank accounts.
3.4 Category position
Product type
Example
Prices issuer risk?
Pays on issuer failure?
Price oracles
Reflector, DIA, Band
No, prices only
No
Capital protection vaults
Cushion (CPPI strategy)
No
Limits portfolio loss, not issuer specific
Optimistic oracles
Soroban Optimistic Oracle
No
No; a tool Sylox reuses
Depeg cover on other chains
DeFi cover protocols on EVM chains
Partly
Yes, but not for Stellar anchors
Sylox
This project
Yes, per issuer
Yes, per issuer, onchain
The Appendix lists prior art and what remains to verify.
4. Architecture
Three components form the protocol: the Risk Oracle turns public data into signals, the Credit Event Registry turns signals and reports into declared events, and Protection Markets pay out on those events. They are built as seven Soroban contracts: the three components (the markets as a factory plus one contract per series), a Staking contract that holds every bond and stake, a Treasury that holds protocol fees, slashed funds and reward pools, and a Governor (technical-doc.md Section 3.1).
Only the shaded area is the protocol. Data sources, readers, reporters and traders are outside participants that anyone can join or replace.
5. Anchor Risk Oracle
The oracle publishes, per issued asset, a set of raw signals plus one open formula risk score. Raw signals come first: the score is a convenience, the signals are the truth.
5.1 Signals
Signal
What it measures
Source
Onchain verifiable?
Peg deviation
Time weighted price vs reference (e.g. USD, EUR or ARS rate), plus the 10th percentile price so one wick cannot move the score
Stellar DEX and AMMs; reference FX feed
Mostly (reference FX needs an oracle)
Liquidity depth
Size of order books and pools within 2% of peg
Stellar DEX and AMMs
Yes
Redemption flow
Net tokens returned to issuer and burned vs issued
Issuer account operations
Yes
Issuer actions
Clawbacks, authorization revoked, flag changes
Issuer account and asset flags
Yes
Supply changes
Sudden mint or burn spikes
Ledger
Yes
Endpoint health
SEP-24 or SEP-6 deposit and withdraw enabled and responding
Anchor's stellar.toml and /info endpoints
No: probed by reporters
Attestations
Reserve reports published by the issuer
Issuer, optionally signed
Partly (signature yes, truth no)
5.2 Risk score
• Signals posted every 5 minutes by default (a governance setting); the score and band are recomputed hourly from confirmed data. Endpoint probes stay hourly.
• Formula published in the repo and stored as a versioned parameter set onchain. Changes go through governance (Section 17).
• Score bands: Normal, Watch, Warning, Distress. A band change emits an event that wallets and lenders can subscribe to.
5.3 How data gets onchain
• Ledger signals: a bonded, permissioned keeper computes from ledger data and posts, with the raw inputs hash; anyone can recompute and dispute. Keepers can backfill missed hours within the 72 hour window, so a short outage leaves no gap.
• Endpoint probes: at least 3 independent reporters probe each anchor from different regions and post signed results. The status is whatever a strict majority reports, Degraded if there is no strict majority, and persistent outliers lose stake. Endpoint status only ever comes from reporters, never from a keeper.
• Reference FX rates: taken from an existing oracle where available (Reflector, DIA, Band and others are on Stellar). Some currencies have two rates: the market (parallel) ARS rate has at times been more than double the official one, so every fiat reference names which one it uses, fixed in the event definition. Which feeds cover ARS and other local currencies, and on which basis, must be checked in Phase 0.
5.4 Coverage at launch
Start with 5 to 10 issued assets that have meaningful supply and an anchor willing to engage, across USD, EUR, ARS and at least one other local currency, chosen from the Phase 0 data pull (technical-doc.md Section 1.7). Add assets by governance, with a minimum liquidity requirement.
6. Credit events
A credit event is the objective, published condition that makes protection pay. Each issued asset has one canonical, versioned definition per event type, set by governance. A market pins the current version of every event type it covers when it opens, so buyers and sellers know the rules before they commit, and nobody can pick a friendlier definition for one buyer. Events of different types are tracked separately: a declared withdrawal halt never blocks a later depeg on the same asset.
6.1 Event types
Event
Definition (draft)
Tier
Depeg
Time weighted price below 0.95 of reference for 72 hours, with up to 6 of the 72 hourly readings allowed missing, and liquidity above a minimum in the week before the 72 hours began
1: data verified (keeper posted, publicly recomputable)
Issuer freeze
Issuer revokes authorization or claws back from more than X% of holders or supply within 7 days, outside a declared compliance action. Only offered for issuers whose account flags allow revocation or clawback
1: data verified (keeper posted, publicly recomputable)
Mint without backing
Supply rises more than Y% in 24 hours with no matching inflow
1: data verified (keeper posted, publicly recomputable) (flag), 3 to confirm
Withdrawal halt
Withdrawals disabled or failing for 72 hours, shown by reporter probes, user reported SEP-24 withdrawals stuck in pending, and the anchor's own statements where available. The least reliable event type
2: reporters
Insolvency or enforcement
Public insolvency filing, licence revocation, or regulator order against the issuer
3: committee
X and Y are set per asset by governance before the market opens. The first build covers Depeg and Issuer freeze; Withdrawal halt, Insolvency and Mint without backing come in a later build phase (technical-doc.md Section 1.2).
6.2 Tier 1: data verified triggers (keeper posted, publicly recomputable)
Soroban contracts cannot read classic DEX books or account history directly, so bonded keepers post the ledger data (prices, liquidity, flags, supply) with a hash of their inputs, and anyone can recompute and dispute it. Anyone can call propose_tier1(asset, event type), which checks that data against the asset's current definition; if conditions hold, the event is proposed and a short challenge window (24 hours) opens for evidence of manipulation.
6.3 Tier 2: staked reporters
Reporters stake collateral to post a claim, for example "Anchor X withdrawals failing since ledger N." Anyone can dispute by staking. Unresolved disputes escalate to the committee. Withdrawal halt is the least reliable event type, because endpoints can fail without a halt and a halt can hide behind endpoints that still answer, so it is Tier 2 only and its evidence combines reporter probes, user submitted SEP-24 transactions stuck in pending beyond a threshold, and anchor cooperation where available. Reuse the existing Soroban Optimistic Oracle rather than building a new dispute system, if it fits after review.
6.4 Tier 3: determinations committee
• 5 to 7 named members from different organizations (ecosystem builders, risk professionals, at least one legal voice), none tied to the asset in question.
• Decides only on Tier 3 events and escalated disputes; votes and reasons published onchain.
• Must rule within 14 days of a dispute being escalated. If it does not, anyone can close the case with a fixed default: a challenged Tier 1 event is declared (the data already met the definition, so the challenger carries the burden of proof), and a Tier 2 or Tier 3 claim is rejected. All bonds are refunded on a timeout, and missed deadlines count against the committee and are grounds for rotation.
• Members rotate yearly; conflicts must be declared.
6.5 After an event
1. Event declared with a ledger timestamp; new cover on that asset stops, and no new market opens on the asset until governance re-enables it.
2. Settlement window opens (Section 7.5).
3. A cure is possible only if the definition allows it (for example price back above 0.98 within the challenge window); after declaration there is no reversal.
6.6 False positives and negatives
• False positive (payout when no real failure): limited by long windows, multiple sources and the challenge period.
• False negative (real failure, no payout): limited by multiple event types, so a freeze or halt can trigger even if the price holds.
7. Protection markets
Each market is a fully collateralized pool for one issued asset and a fixed set of event definitions: sellers lock USDC, buyers pay premiums, and the pool pays buyers automatically if a covered event is declared.
7.1 Market terms (fixed at opening)
Term
Example
Reference asset
ARS token issued by Anchor X
Events covered
Depeg and issuer freeze in the first build (withdrawal halt and insolvency later), each pinned to its current definition version
Settlement asset
USDC; never the reference asset itself or another asset from the same issuer
Term
30 or 90 days, rolling series
Payout type
Fixed (v1): 100% of cover; recovery based (v2)
Max cover per market
Capped by liquidity depth (Section 8)
7.2 Sellers (protection writers)
• Deposit USDC into a market series and receive a seller share token.
• Collateral is locked until the series ends, or paid out on an event.
• Earn premiums pro rata. Can exit early only by selling the share token, never by withdrawing locked collateral.
7.3 Buyers
• Choose amount of cover and term; pay the full premium upfront in USDC.
• Receive a transferable protection token for that series.
• No need to hold the reference asset (see legal note in Section 11 on whether to require it). Where a market does require it, the holding is valued in USD at the current peg and FX rate.
• No buying into a failure already under way: cover cannot be bought while any hourly price in the last 72 hours was below the depeg threshold, endpoints were down or degraded during the halt window, the issuer clawed back or revoked in the last 7 days, or any event is in progress.
7.4 Pricing (v1)
Sellers post rates (annual % of cover) in an order book; buyers take the cheapest available. No pricing model is trusted at launch. A model based reference rate is published from oracle data once enough history exists.
7.5 Settlement
• Event declared: each protection token redeems for its cover amount in USDC from the pool, during a 30 day claim window; sellers' collateral covers it.
• Which events count: an event covers a market if the failure started during the term. Because a depeg needs 72 hours of data before it can be proposed, a proposal is still accepted up to one event window (72 hours for a depeg) after expiry, and the market waits in a pending state meanwhile. A depeg that starts on day 88 of a 90 day market therefore still pays.
• No event by expiry: once that waiting period ends, sellers withdraw collateral plus premiums; protection tokens expire worthless.
• Cash only: no delivery of the failed token, because a frozen or clawed back token may not be transferable.
7.6 Contract interfaces (draft)
fn open_series(env, terms: SeriesTerms) -> Address; // terms pin one definition version per covered event type
fn deposit(env, seller: Address, amount: i128); // on the series contract
fn quote(env, seller: Address, rate_bps: u32, available: i128);
fn buy_cover(env, buyer: Address, amount: i128, max_rate_bps: u32, max_premium: i128) -> (i128, i128); // (filled, premium); refunds max_premium minus premium
fn claim(env, holder: Address, amount: i128) -> i128;
fn withdraw(env, seller: Address, amount: i128) -> i128;
fn event_status(env, asset: Address, kind: EventKind) -> AssetEventStatus; // from the Credit Event Registry, per asset and event type
The full reference is technical-doc.md Section 12.
7.7 Fees
A small protocol fee on premiums (target 5 to 10% of premium, set by governance) is paid into the protocol Treasury contract, which funds keeper and reporter rewards, the committee and maintenance. Slashed bonds also go to the Treasury. No fee on collateral or payouts.
8. Pricing and risk limits
The single biggest design risk is manipulation: on a thin market, a protection buyer can push the price down to trigger their own payout. Every limit below exists to make that unprofitable.
8.1 Manipulation defences
Defence
How it works
Long windows
Depeg requires 72 hours below threshold, not a single trade; the score uses the 10th percentile price, so one wick cannot move a band
Multiple sources
Price must break on Stellar DEX, AMMs and an external reference where one exists
Liquidity floor
Depeg only counts if depth near peg was above a minimum in the week before the 72 hours began, so a book that was always empty cannot trigger. A collapse in depth during the failure is itself a warning signal, never a reason to block a payout
No informed buying
Cover cannot be bought while a failure signal is already present (Section 7.3), and an event only covers markets whose term contains the start of the failure
Cover cap
Total open cover per asset is capped at a fraction of measured liquidity depth (start at 25%), so pushing the price costs more than the payout
Challenge window
24 hours to submit evidence of wash trading before an event is final
Multiple event types
Real failures usually show in issuer actions or endpoints too, so the protocol does not depend on price alone
8.2 Correlation and pool safety
• Pools are isolated per asset and per series. One anchor failing cannot drain another pool.
• No cross margining in v1.
• A seller who wants diversified exposure deposits into several pools on purpose.
8.3 Settlement asset risk
Payouts are in USDC, which has its own issuer. Document this openly. Later options: let markets choose the settlement asset, or offer an XLM settled series.
8.4 Concentration limits
• Max cover per buyer per series (start: 10% of the series cap) to stop one party dominating.
• Optional limit on sellers related to the reference anchor, since an anchor insuring itself does not spread risk.
8.5 Data for later pricing
The oracle's history (deviations, liquidity, redemption flow) is the dataset a pricing model needs. Publish it openly from day one; it is also useful on its own to researchers and lenders.
9. Participants and core journeys
Six kinds of participants use the protocol; none needs permission from the team.
Participant
Uses
Gets
Protection buyer (holders, NGOs, treasuries, lenders)
Buys cover on an issued asset
Payout if the issuer fails
Protection seller (yield seekers, market makers)
Locks USDC in a series
Premiums
Reader (wallets, lending protocols, dashboards)
Reads risk scores and event flags
Shared, objective issuer risk data
Reporter
Probes anchor endpoints, posts claims
Fee share; loses stake if wrong
Committee member
Rules on Tier 3 events and disputes
Fee share; public accountability
Anchor
Engages with the protocol, publishes attestations
A public, verifiable safety signal
9.1 Buyer journey
1. NGO treasurer holds 50,000 USD worth of an ARS token for local payouts.
2. Opens the Sylox app, sees the issuer's score (Normal, 18/100) and its signals.
3. Buys 90 days of cover for 20,000 USD at the best quoted rate, paying the premium in USDC.
4. Holds a protection token in their wallet.
5. If the issuer halts withdrawals and the event is declared, the treasurer claims 20,000 USDC. If not, the cover expires.
9.2 Seller journey
1. A market maker reviews the asset's signals and history.
2. Deposits 100,000 USDC into the 90 day series and quotes a rate.
3. Earns premiums as buyers take cover; withdraws collateral plus premiums at expiry, or sells the share token early.
9.3 Reader journey (lending protocol)
1. A lending pool reads the risk band for each issued asset it accepts.
2. On Warning, it lowers the collateral factor; on Distress or a declared event, it pauses new borrowing against that asset.
3. No custom risk team required for each issuer.
10. Security and threat model
The protocol holds real collateral and decides payouts, so its threats are both technical (contract bugs) and economic (gaming the triggers).
Threat
Example
Defence
Trigger manipulation
Buyer dumps the token to force a depeg payout
Long windows, liquidity floor, cover caps, challenge window (Section 8)
False reports
Reporter claims a halt that did not happen
Staked reporters, majority of 3+, disputes, slashing
Committee capture
Members collude on a ruling, or never rule
Diverse members, conflict rules, public reasons, rotation; only Tier 3 events and escalated disputes; a 14 day ruling deadline with a fixed default outcome
Oracle keeper failure
Feed stops updating
Several bonded keepers can post and backfill missed hours; anyone can recompute and dispute; staleness flag pauses new cover
Contract bug drains collateral
Accounting error in claims
Isolated pools, invariant tests, fuzzing, audit via the SCF Audit Bank before mainnet
Issuer front running
Anchor sells cover knowing it will fail
Optional limits on related sellers; all positions public
Settlement asset freeze
USDC frozen for the pool contract
Disclosed risk; future multi asset settlement
Governance attack
Changing event definitions mid series
Definitions fixed per series; changes apply only to new series, with a timelock
10.1 Process
• Threat model and monitoring plan delivered at Tranche #2 (SCF requires both at that tranche).
• Monitoring: alerts on score band changes, stale feeds, reporter disagreement, unusual cover purchases before price moves, and pool invariant breaks.
• Audit through the SCF Audit Bank before mainnet, covering all seven contracts.
• SECURITY.md, disclosure process, and a bug bounty once mainnet collateral is meaningful.
11. Legal and compliance plan
The protection market is likely a derivative or insurance product in most jurisdictions, so mainnet launch of the market is gated on a legal opinion. This section lists the questions to answer; it is not legal advice.
11.1 Questions for counsel
• Is cover on a single issuer a swap, a security based swap, an insurance contract, or something else, in the US, EU, UK, Nigeria and other target markets?
• Does requiring buyers to hold the reference asset (an "insurable interest") change the classification?
• What restrictions apply to the protocol contracts, the web app, the team and the foundation or company that deploys them?
• Can sellers be anyone, or only qualified participants?
• How should terms, risk disclosures and naming be written? Avoid the word "insurance" unless the structure is insurance.
• Does publishing a risk score about a named company create defamation or rating agency exposure?
11.2 Product decisions that reduce risk
Decision
Effect
Risk feed is pure public data, no investment advice
Lowest risk component; can launch first
Fully collateralized, no leverage
Avoids margin lending issues
Optional insurable interest requirement per market
Keeps the insurance route open
Geo blocking of restricted jurisdictions in the app
Reduces exposure in high risk regions
Open formula, raw signals shown with every score
Positions scores as data, not ratings
Anchors can comment on their page
Fairness and dispute path for named companies
11.3 Sequencing
1. Testnet: everything, no real money.
2. Mainnet: risk feed and credit event registry (data only).
3. Mainnet: protection markets, only after a written legal opinion and with the restrictions it requires.
The SCF Handbook says some projects may get a modified tranche structure or a testnet only deployment due to jurisdiction. Raise this with the SCF team early rather than at review.
12. Integrations and ecosystem
The protocol succeeds when other projects read its feed and route users to its markets, so integrations target readers and liquidity, not only end users.
Role
Target type
What they do
Proves
Reader: wallet
A Stellar wallet or Stellar Wallets Kit
Shows risk band next to issued assets
Distribution of the feed
Reader: lender
A Stellar lending protocol that accepts issued tokens as collateral
Adjusts limits from risk bands
Composability
Reader: auto switch
A tool that rotates out of a failing issuer (idea #20)
Acts on Distress or event flags
Downstream protocol
Anchor
2 to 3 anchors with meaningful supply
Engage, publish signed attestations, comment on their page
Supply side trust
Seller liquidity
Market maker or treasury
Seeds first series
Market depth
Buyer
NGO or fintech treasury holding local tokens
Buys first cover
Real demand
Oracle infra
Existing price oracles and Soroban Optimistic Oracle
Reference FX and dispute layer
Reuse, not rebuild
12.1 Pre application evidence (target)
• Risk feed live on testnet for 5+ issued assets, using real mainnet data.
• 1 wallet or lender committed to read the feed (letter of intent).
• 2 anchors aware and engaged (letters or public comment).
• 1 seller and 1 buyer committed to testnet trials.
• Written scoping call with legal counsel.
12.2 Anchor relationships
Anchors may see a public risk score as a threat. Frame it as a way for good anchors to prove safety, give them a right of reply, and publish the formula. Engage before launch, not after.
13. Onchain metrics
The headline metric is total value locked as protection collateral (TVL), backed by active cover and feed usage, all read directly from the contracts.
Metric
Source
Why it matters
Collateral locked (USD)
Pool contracts
Primary growth metric; Tranche #3 target
Active cover outstanding (USD)
Protection tokens
Real demand for protection
Premiums paid (USD, 30 day)
Pool contracts
Market is live, not idle capital
Issued assets covered
Oracle registry
Breadth of the feed
Contracts reading the feed
Calls to oracle getters
Composability
Independent reporters active
Staking contract
Decentralization of data
Unique buyers and sellers
Positions
Not one or two whales
13.1 Tranche #3 target (draft)
To propose to the panel, sized for a new market on small issued token supplies:
• $100K collateral locked across at least 3 issued assets, held for a 30 day window
• at least 10 unique buyers and 5 unique sellers
• at least 1 external contract reading the feed on mainnet
If the legal opinion restricts mainnet markets, the fallback target is feed adoption: 3 external readers and 10+ assets covered on mainnet. Agree this fallback with the panel upfront.
13.2 Transparency
All contract ids registered at award; a public dashboard shows every metric; team or related party positions are flagged and excluded.
14. Pre SCF roadmap
Legal scoping comes first and partners come before the application, because those decide whether the protocol can launch at all.
Week numbers are relative to the start of Phase 0; the SCF #47 deadline is still to be confirmed. Failing a gate triggers the kill criteria in Section 18. Phase 0 includes a mainnet data pull of candidate assets (USD, EUR, ARS and others) with liquidity, trade history and issuer flags, which decides the launch assets and whether the cover cap is meaningful (technical-doc.md Section 1.7).
15. SCF Open Track alignment and tranche plan
Sylox fits the Open Track's "novel protocols or primitives that solve for key ecosystem needs": it is new on Stellar, fully onchain, and built for others to use. Tranches follow SCF's 10 / 20 / 30 / 40 split and end at mainnet.
15.1 Open Track checklist
Handbook item
Where answered
Novel protocol, not under other tracks
Sections 3, 19
Technically ambitious, clearly scoped
Sections 5 to 8
Onchain integration plan for the Stellar stack
Sections 4 to 7
Milestones mapped to mainnet
Section 15.2
Thoughtful budget
Section 16
Onchain growth and how it is measured
Section 13
AI disclosure
To add: AI_DISCLOSURE.md
Market analysis
Sections 2, 3.4, 12
Unified docs (GitBook)
To set up in Phase 1
Team track record
Team section of the application; add a risk or quant advisor
Uses the network's latest strengths
Soroban, issuer asset flags, DEX and AMM data, contract events
Team video
To produce
15.2 Tranches
Tranche
Share
Deliverables
Evidence
#0
10%
Award acceptance
n/a
#1 MVP
20%
Risk oracle with keeper posted, recomputable signals for 5+ assets; credit event registry with Tier 1 triggers for Depeg and Issuer freeze on canonical definitions; protection pools on testnet; web app alpha
Testnet contract ids, public feed, test suite, demo video
#2 Testnet
30%
Tier 2 reporters and disputes; committee process with ruling deadline; endpoint probes; seller order book; manipulation limits; threat model and monitoring plan
Testnet trials with real buyers and sellers, dispute drill, threat model doc
#3 Mainnet
40%
Mainnet risk feed and event registry; mainnet protection markets if the legal opinion allows; wallet or lender reading the feed; metric target (Section 13.1)
Mainnet contract ids, dashboard over the agreed window
Each tranche is submitted within 90 days of the previous payment. Notify the SCF team before any deadline that will slip.
15.3 Questions to ask the SCF team before applying
• Is a protection market acceptable in the Open Track, and under what jurisdiction conditions?
• Would they accept the fallback Tranche #3 metric if markets are restricted to testnet?
16. Budget
Draft ask: $145K in XLM, close to the $150K lifetime cap per project, reflecting the full protocol scope. Figures are placeholders until team size and rates are set.
Line item
USD
Tranche
Protection market contracts: pools, order book, settlement, claims
35,000
#1, #2
Risk oracle: signal contracts, keeper, score formula
30,000
#1, #2
Credit event registry: triggers, reporters, disputes, committee tooling
25,000
#1, #2
Web app and SDK for wallets and lenders
18,000
#1, #3
Reporter network and endpoint probes, 12 months of operation
12,000
#2, #3
Risk and quant advisor: event definitions, limits, pricing data
12,000
#1, #2
Indexer, public dashboard, monitoring
8,000
#2, #3
Spec, docs, governance setup
5,000
#1 to #3
Total
145,000

16.1 Not in the budget
• Seed liquidity: collateral for first pools comes from partners, never from award funds.
• Marketing, incentives, liquidity mining: none; they would also distort the metrics.
16.2 Sizing note
An ask near the cap invites tougher review. If the SCF team signals concern, split the scope: risk feed and event registry in the first award, protection markets in a follow on award once the legal opinion exists.
17. Governance and sustainability
Governance controls parameters, never open positions: rules for a series are fixed when it opens, so no vote can change a payout already bought.
17.1 What governance can change
Item
Who decides
Safeguard
Add or remove covered assets
Multisig, later token or delegate governance
Minimum liquidity rule; 7 day notice
Event definitions for new series
Multisig with risk advisor sign off
One canonical version per asset and event type; never applies to open series; a new version is registered only after markets on the old one have ended
Score formula version
Multisig
Published diff; old version kept for history
Protocol fee
Multisig
Capped (e.g. 10% of premium)
Treasury allocation and spending
Multisig
Timelocked; only protocol fees and slashed funds, never collateral, bonds or stakes
Committee membership
Multisig plus committee
Yearly rotation; conflicts declared; missed ruling deadlines are grounds for rotation
Contract upgrades
Multisig (team + external signers)
14 day timelock; pools isolated; market contracts never upgraded
17.2 Decentralization path
In v1 the trust root for payouts is the permissioned, bonded keepers plus the 4 of 7 committee (Section 1.4a). The path away from it:
1. Testnet and early mainnet: team multisig with at least 2 external signers.
2. Later versions: open keeper and reporter registration with higher stakes, instead of governance approval.
3. Later versions: onchain verifiable signal disputes, replacing committee recomputation where the data can be proven onchain.
4. After 12 months: move parameter control to a broader council (anchors, readers, sellers).
5. Freeze core pool contracts; new versions deploy alongside, users choose.
17.3 Sustainability
• Protocol fee on premiums goes to the Treasury contract and pays keeper and reporter rewards, the committee and maintenance.
• The risk feed is a public good and can seek the SCF Public Goods Award (up to $50K per quarter, invitation only).
• Code Apache-2.0; formula and event definitions public.
17.4 If the team stops
Open series settle by their fixed rules; reporters and committee keep their roles as long as fees fund them; the feed can be run by anyone using the open formula.
18. Risks and kill criteria
The top two risks are legal classification and a market with too little liquidity on either side; both are decided outside the code.
18.1 Risks
Risk
Likelihood
Impact
Mitigation
Legal: markets restricted or blocked
High
High
Feed first; legal opinion before mainnet markets; fallback metric agreed with SCF
No sellers or no buyers
High
High
Line up both before applying; start with few assets; partner treasuries
Trigger manipulation
Medium
High
Section 8 limits; challenge window
Anchors hostile to public scores
Medium
Medium
Right of reply, open formula, engage early
Thin issued token supply limits cover size
High
Medium
Cover caps; start with larger assets; frame early size honestly
Wrong or disputed event ruling
Medium
High
Clear definitions; tiers; committee with public reasons
Reviewers see scope as too large
Medium
Medium
Ready to split per Section 16.2
Settlement asset (USDC) freeze
Low
High
Disclosure; multi asset settlement later
Team lacks risk and legal expertise
High
High
Add a risk advisor and counsel before applying
Someone else ships it first
Unknown
Medium
Public post in Phase 0; prior art kept current
18.2 Kill criteria
Checkpoint
Stop or pivot if
After Phase 0
Counsel says markets cannot launch anywhere useful; or the SCF team says it is out of scope
After Phase 1
Onchain signals cannot be computed reliably or cheaply for 5 assets
After Phase 2
No anchor engages and no wallet or lender wants the feed
After testnet trials
Fewer than 3 sellers and 5 buyers participate, or manipulation tests succeed
Before mainnet markets
No written legal opinion, or Audit Bank finds unresolved critical issues
Most likely pivot: keep the Anchor Risk Oracle and credit event registry as public infrastructure, and drop or postpone the protection markets.
19. Appendix: prior art, open questions, sources
A search on October 8, 2026 of the Stellarlight and LumenLoop directories found no Stellar project that scores issuer risk per anchor or sells protection against issuer failure. These sources cover public, indexed work only.
A. Prior art and neighbours
Project
What it does
Relation
Cushion
CPPI capital protection vaults on Soroban
Portfolio protection, not issuer specific
Soroban Optimistic Oracle
Decentralized dispute and arbitration
Candidate dispute layer for Tier 2
Reflector, DIA, Band, Pyth, RedStone
Price oracles on Stellar
Reference prices; FX coverage to check
Usher
Verifiable offchain payment rail data for anchors
Possible data partner for endpoint and rail health
Clear
Liquidity from guarantors for payment companies
Adjacent: guarantor capital for operators
OrbitCDP, Lucent
CDP stablecoins on Stellar
Possible readers of risk bands
Depeg and cover protocols on EVM chains
Cover against stablecoin or protocol failure
Concept exists elsewhere; not for Stellar anchors
B. Open questions
[ ] Is the name "Sylox" free (projects, domains, npm)?
[ ] Which oracles provide ARS and other local currency reference rates on Stellar, and do they publish the official rate, the market rate, or both?
[ ] Which issued assets have enough supply and liquidity to cover at launch? (Phase 0 data pull, technical-doc.md Section 1.7)
[ ] Legal classification in target jurisdictions (Section 11.1)
[ ] Does SCF accept a protection market in the Open Track, and on what conditions?
[ ] Who joins as risk or quant advisor, and who sits on the first committee?
[ ] Does the Soroban Optimistic Oracle fit Tier 2 after a technical review?
[ ] SCF #47 deadline date?
C. Sources
• SCF Handbook: Open Track
• SCF Handbook: Public Goods Award
• SCF Handbook: Welcome and changelog