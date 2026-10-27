# cmux-next state placement (DO vs PlanetScale MySQL), MySQL migration, new Cloud backend (design, 2026-10-04)

Status: design for review (backend lead). No code until the coordinator and Lawrence accept it.
It replaces the Postgres projection decision and the classic Cloud dependency. Facts used:
Vitess database `cmux-next-vitess` (org cmux, us-west-2, PS-10, branches main/staging/development);
Postgres `cmux-next` stays only until the copy is verified, then is retired with Lawrence's OK;
classic `cmux-prod` is never touched. Identity stays Stack Auth (prod 9790718f, dev 454ecd03).
Usage history goes to ClickHouse. Billing uses the classic Stripe account and products (one
subscription). Freestyle: same account, new keys `cmux-next-dev` and `cmux-next-prod`.

## 1. The rule

A fact lives where its single writer is.

- **Durable Object SQLite** holds state that one entity owns and that must be strongly
  consistent: the single writer (ordering, invariants, quotas checked at write), the idempotency
  ledger, realtime fan-out (sockets, events), revocation, alarms, and provider calls (crash-safe
  ledger rows). Reads by the owner and its members are served here.
- **PlanetScale MySQL** holds facts that are about many entities at once: cross-entity queries
  (all teams of a user, all machines of an org for admin), directory and search, global uniqueness
  that no one entity owns (email address claims, slugs, domains), billing summaries, admin and
  support reporting, and data an operator must query with SQL. MySQL rows are written only by the
  owning DO through the outbox (projection), or by Stripe webhooks for billing. MySQL is never
  the authority for a decision that a DO makes; a DO never reads MySQL inside a commit.
- **ClickHouse** holds append-only usage and audit history at volume (per-run, per-minute VM
  usage, analytics). MySQL keeps only monthly billing summaries.
- **R2** holds blobs (attachments, archived event windows, snapshot exports, migration files).

A projection row carries `(source_stream, source_seq)`; an upsert applies only when it is newer,
so outbox replay is safe and the whole projection can be rebuilt from the DOs.

## 2. Placement per domain

| Domain | DO (authority) | MySQL (projection or authority) | Why |
| --- | --- | --- | --- |
| Users (profile, settings, installs, grants, presence keys) | UserDO | `users` (id, stack_user_id, email_hash, created) ; `installs` (id, user, kind, revoked) | Revocation and grants are decided per request by UserDO (instant revocation). MySQL is for "find user by Stack id / email" and admin. |
| Email/phone address claims | AddressDO (per address) | `address_claims` (unique address hash -> user) | The DO serializes one address; MySQL gives the global unique index for lookup and admin. |
| Teams, members, hosts, policy, SSO, domains | TeamDO (row mode) ; DomainDO | `teams`, `memberships` (team, user, role), `hosts`, `domains` (unique) | Authorization reads member rows in the TeamDO. MySQL answers "teams of a user" for directory UIs and admin; the UserDO `team_index` stays the hot path for reach. |
| Conversations, messages, reads, attachments refs | ConversationDO | `home_conversations`, `home_participants`, `home_invites`, `home_message_search` | Messages need ordering and realtime. Search is cross-conversation, so it is MySQL (section 4.3). |
| Attachments bytes, quota | ConversationDO record + UserDO quota | none (R2 holds bytes) | Quota is a per-user invariant. |
| Inbox, unread | UserDO inbox engine | none | Per-user, realtime. |
| Scheduler (automations, runs) | SchedulerDO (rows, (g)) | `automations`, `automation_runs` (finished history), billing summary per month | Cron/dedupe/token bucket need one writer. Run history beyond the kept 200 is a cross-time query. Per-run usage detail goes to ClickHouse. |
| Connections (OAuth, sealed credentials) | ConnectionDO | `connections` (id, team, provider, account, status; never secrets) | Secrets never leave the DO. |
| Feed | FeedDO | none except daily counts if needed | Per-install stream. |
| Audit | owner DOs (event window) | `audit_events` (append, retention) and ClickHouse for long history | Compliance queries cross entities. |
| Cloud machines, snapshots, quotas | CloudDO (per team, section 5) | `cloud_machines`, `cloud_snapshots` (projection for admin, support, migration reports) | Quota and provider-call ledger need one writer per team. |
| Billing (plan, seats, entitlements) | UserDO/TeamDO cache of the entitlement (written by the billing webhook op) | `billing_customers` (stripe customer <-> team/user, unique), `billing_subscriptions`, `billing_usage_monthly` | Stripe is the source; the webhook writes MySQL (global uniqueness of the Stripe customer) and sends an op to the owner DO, which keeps the entitlement it checks at write. |
| Usage metering | UsageMeterDO (hard cap, token bucket) | `billing_usage_monthly` | Detail rows go to ClickHouse. |
| Idempotency ledgers, outbox, event windows | every OwnerDO | none | Per-entity by definition. |

Rejected alternatives. Member lists only in MySQL: authorization would read MySQL inside a
commit (not consistent, adds a network hop to every op). Search in a DO: cross-conversation
search would fan out to every ConversationDO. Billing only in a DO: the Stripe customer id must
be globally unique and the webhook has no owner context until it is looked up.

## 3. Postgres usage today (inventory)

All relational code in the cmux-next backend is Postgres:

| Place | What | Postgres-specific |
| --- | --- | --- |
| `backend/db/schema/index.ts` (242 lines) | Drizzle `pg-core` schema: users, teams, automations, installs, hosts, automation_runs, connections, home_conversations, home_invites, memberships, home_participants, audit_events | `pgTable`, `timestamp with time zone`, `jsonb`, `text` keys |
| `backend/db/migrations/0001..0006*.sql`, `backend/db/drizzle/0000_baseline.sql` + `meta/` | hand SQL + Drizzle baseline | `pg_trgm`, `btree_gin`, GIN full-text and trigram indexes, `home_message_search` hash-partitioned (8 parts), `timestamptz` |
| `backend/db/migrate.ts` (+ `libpg-query` lint), `db/e2e.ts`, `db/drizzle.config.ts` | migration runner, lint, e2e | `pg`, `libpg-query`, `schema_migrations` table |
| `backend/db/test-pg/projection.test.ts`, `home-search.test.ts` | DB tests against Postgres | Docker Postgres |
| `backend/apps/api/src/projection.ts` (125) | raw SQL upserts/deletes for message search, dead-row replay | `ON CONFLICT ... DO UPDATE ... WHERE source_seq < excluded.source_seq`, `$n` params |
| `backend/apps/api/src/projection-drizzle.ts` (209) | Drizzle upserts for the other tables | `onConflictDoUpdate` with a `where` guard, `PgTable` |
| `backend/apps/api/src/home-search.ts` (135) | `home.search` read (read-only Hyperdrive) | `ILIKE ... ESCAPE`, `to_tsvector`/GIN, row-value keyset `(a,b,c) < ($1::timestamptz,$2,$3)` |
| `backend/apps/api/src/feed-sweep.ts` (89) | user paging | `$1::text IS NULL` |
| `backend/apps/api/src/env.ts`, `wrangler.jsonc` | `HYPERDRIVE`, `HYPERDRIVE_RO` per env (6 Hyperdrive configs to Postgres) | |

Data in Postgres `cmux-next` today (read-only count, 2026-10-04): production only
`schema_migrations`; staging about 40 rows (12 installs, 10 connections, 5 automations, 5 runs,
3 hosts, 1 conversation, 1 invite, 2 participants, 1 team, 1 user, 1 membership); development 7 rows.

## 4. MySQL migration (proper migration, not rewrite-and-hope)

### 4.1 Schema and Drizzle

- Drizzle moves to `drizzle-orm/mysql-core` with the `mysql2` driver (Hyperdrive MySQL). One schema
  file per area under `backend/db/schema/`. Types: ids `varchar(64)` (our ids are `user_<20 hex>`,
  `team_...`), `ascii_bin` collation for ids and hashes (byte comparison, smaller indexes),
  `utf8mb4_0900_ai_ci` for human text; timestamps `datetime(3)` in UTC (MySQL `timestamp` ends in
  2038); JSON columns `json` (only for opaque payloads, never queried by path in a hot path);
  booleans `tinyint(1)`; `bigint` for seq and money (micros).
- Vitess rules: no foreign keys (integrity comes from the single-writer DOs; orphan checks run in
  the verification job); every table has a primary key; no stored procedures, triggers or
  partitioning in SQL (Vitess shards later by keyspace instead: the shard key is `team` for team
  tables and `user` for user tables, so every hot query filters by it); online DDL through
  PlanetScale deploy requests (safe migrations on for main), never ad hoc `ALTER` on main.
- Migration history: a fresh MySQL history. `0000_baseline` = the current schema translated
  table by table (generated by `drizzle-kit generate` from the mysql-core schema, then reviewed by
  hand). Each later change is one Drizzle migration plus a deploy request per branch. The old
  Postgres history is archived under `backend/db/postgres-archive/` until the database is retired.
- Upsert guard: `INSERT ... ON DUPLICATE KEY UPDATE col = IF(VALUES(source_seq) > source_seq,
  VALUES(col), col), ...` with `source_seq` assigned last in the list (MySQL evaluates the
  assignments left to right). One helper builds this so every projection uses the same guard;
  tests cover older, equal and newer seq. Deletes keep the `source_seq <=` guard.

### 4.2 Queries

Every query in section 3 is ported: `$n` -> `?`; row-value keyset becomes
`(created_at < ? OR (created_at = ? AND (conversation_id < ? OR (conversation_id = ? AND seq < ?))))`
with a covering index; `IS NULL` paging as is; `ILIKE` becomes `LIKE` on a `utf8mb4_0900_ai_ci`
column (case- and accent-insensitive).

### 4.3 Search on MySQL

`home_message_search` gets a `FULLTEXT` index with the `ngram` parser (works for CJK, the reason
`pg_trgm` was used), queried `MATCH(body) AGAINST(? IN BOOLEAN MODE)` filtered by the caller's
conversation ids (the Worker passes the ids the user's inbox lists; never a cross-team scan).
Substring fallback for one- and two-character queries: `LIKE` on the recent window only.
Risk: Vitess supports FULLTEXT in an unsharded keyspace; when we shard, search moves to its own
unsharded keyspace or to a search service. Test: the existing home-search test cases (CJK,
punctuation, keyset) must pass on MySQL.

### 4.4 Hyperdrive, harness, CI

- One MySQL Hyperdrive config per environment and per role: `cmux-next-vitess` development,
  staging, main, each with a read-write credential (projection writer) and a read-only credential
  (reads such as home.search). Bindings keep the names `HYPERDRIVE` and `HYPERDRIVE_RO`, switched
  per environment only after verification (4.5). Credentials are created with
  `pscale password create cmux-next-vitess <branch> --org cmux --role writer|reader`, stored in
  `~/.secrets/cmux-next-vitess-<branch>-{rw,ro}.env` (600) and in the Hyperdrive config, never
  printed.
- Local and CI: a `mysql:8.4` container for unit and projection tests (fast, offline) plus one CI
  job against the PlanetScale development branch (catches Vitess-only differences). The test-pg
  suite becomes test-mysql with the same cases.
- Runner: `db/migrate.ts` applies Drizzle migrations to development and staging; main changes go
  through deploy requests only.

### 4.5 Data move and cutover (per environment: development, then staging, then main)

1. Create the schema on the Vitess branch (baseline migration).
2. Rebuild, not copy, as the primary path: every projection row is derived from a DO, so the
   backend replays the projection into MySQL from the DOs (admin op `projection.rebuild {target}`
   that walks owners and re-emits their projection items with the current seq). This proves the
   projection code on MySQL and needs no Postgres-to-MySQL type mapping.
3. Verified copy as the check: an export job reads each Postgres table (read-only) and the MySQL
   table, and compares row counts and a per-row checksum (sha256 of the canonical JSON of the row,
   normalized types). Any difference blocks the switch. Tables that are not rebuildable from DOs
   (none today; audit_events is rebuildable only inside the event window) are copied row by row
   by the same job, then checked.
4. Dual write window: for one day per environment the outbox writes both Postgres and MySQL
   (two projection targets, same guard); a compare job runs every hour and must show 0 differences.
5. Switch `HYPERDRIVE`/`HYPERDRIVE_RO` to MySQL for that environment (one Worker deploy). Reads
   now come from MySQL; Postgres still receives writes for 3 more days.
6. Rollback: switch the bindings back (one deploy); Postgres is current because dual write
   continued. After 3 clean days, stop the Postgres writes.
7. After all three environments are done and verified, ask the coordinator (and Lawrence) once
   more, then delete the Postgres `cmux-next` database and its 6 Hyperdrive configs.

Production holds no projection rows today, so main is the lowest-risk step; staging is the real
rehearsal.

## 5. New Cloud backend (Cloudflare Worker/DO <> Freestyle)

No dependency on `web/`, Vercel, classic Postgres `cloud_vms`, the GCP edge or classic WireGuard.

### 5.1 Objects

- **CloudDO, one per team** (a personal account is a team of one): the machine and snapshot
  registry (rows, row mode), the provider-call ledger, quotas and plan checks, idle policy, and
  the alarm that repairs interrupted provider calls and polls provider state. Teams have few
  machines (plan max_active is 5 to 50), so one writer per team is cheap and makes quota checks
  exact. Provider calls run single-flight per machine (not per team), like TeamVmDO.
- **UserDO**: identity, install revocation, chief/agent refusal for destructive and money ops.
- **TeamDO**: member role and Cloud policy (who may create, max size), read by CloudDO through
  authorize rows (member) or an RPC cached per request.
- **MySQL** `cloud_machines`, `cloud_snapshots`: projection for admin/support/migration reports.

### 5.2 Provider-call ledger (answers contract 1.6)

1. A mutation commits a ledger row first: `{op, key, machine id, provider name, state: pending}`.
   The provider name is deterministic: `<env prefix><machine id>`.
2. CloudDO then calls Freestyle (single flight per machine) and commits the outcome as an internal
   op (`cloud.driver_result`). A crash between the call and the commit is repaired by the alarm:
   it looks the resource up by its deterministic name, so a lost create answer never makes a
   second VM.
3. A call cut off mid-flight (timeout, Worker eviction) answers `mutation.indeterminate`; the
   client retries with the same key; the retry reads the ledger row and resumes or returns the
   stored result. A retry with a different key is a new intent.
4. Deletes are idempotent: a provider 404 on delete is success; the row stays as a tombstone 30
   days so a retry answers `{deleted: true}`.

### 5.3 Names, keys, account isolation

Same Freestyle account as classic, new keys. Env prefixes: `cmuxnp-dev-`, `cmuxnp-stg-`,
`cmuxnp-prod-` (not `cmux-`: classic already uses `cmux-` names on this account, and a prefix
must identify our resources for cleanup). The driver refuses any call on a resource whose name
does not carry its environment prefix, except classic machines imported by migration (allowlist
by provider id, written by the import). Keys: Worker secrets `FREESTYLE_API_KEY` per environment
(`wrangler secret put`), source of truth `~/.secrets/cmux-next-freestyle-{dev,prod}.env`; no client,
VM, app server or log ever sees one. Staging uses the dev key with the `cmuxnp-stg-` prefix unless
Lawrence wants a third key.

### 5.4 Ops

I accept the contract's op set (section 1.3) and names `cloud.machine.*`, `cloud.snapshot.*`,
`cloud.plan.get`, `cloud.billing.checkout`, `cloud.shell.open`, `cloud.migration.*`,
`cloud.machine.upgrade`, owner `cloud:CloudDO`, with these changes:
- Agent principals (claim `agt`) are refused for create, delete, resize up, snapshot delete,
  billing checkout and migration start (backend-enforced, not only the client).
- `cloud.machine.list` pages (`cursor`, `limit<=100`) for large teams; first page is the default.
- Add `cloud.machine.exec {machine, argv, timeout_s}` only for the upgrade path (internal), not
  public in v1.

### 5.5 Credential: install token, not Stack bearer

Yes. The backend already authenticates install tokens on every request with instant revocation
through UserDO. The daemon relay adds the install token; the Stack session token stays in the
browser/app sign-in path only.

### 5.6 Exec / terminal stream relay through the Worker

Feasible. `cloud.shell.open` returns a stream id; the client opens a WebSocket to the Worker; the
Worker authenticates (install token, machine membership via CloudDO) and opens the Freestyle exec
WebSocket with the key, then pipes frames both ways. A plain Worker request (not a DO) does the
relay, so it costs wall time only, not CPU, and needs no DO memory. Limits: one relay per stream,
idle close after 10 minutes, max 4 per machine. Normal terminals use the daemon link; the relay is
for rescue shell and classic machines before upgrade.

### 5.7 Quotas and billing

CloudDO checks the team entitlement (plan, max_active, max_saved, sizes) before any provider call.
The entitlement comes from the billing webhook path (section 2: Stripe -> MySQL billing tables ->
op to the owner), never from the request. VM-hours: CloudDO emits usage events per state change
to UsageMeterDO (hard cap) and ClickHouse (detail); MySQL holds the monthly summary.

### 5.8 VM bind and `cloud.machine.connect_info` (contract 1.7)

1. Create: CloudDO mints the machine id, the overlay host id (`host_...`, stable for the machine's
   life) and epoch 1, and a one-time bind token (32 random bytes; only its sha256 is stored; expires
   15 minutes after the provider reports running). The token goes into the VM at create (Freestyle
   file write), never into a log or the client.
2. Bind: the image's bind agent calls internal op `cloud.machine.bind {machine, bind_token,
   wg_public_key, daemon: {version, capabilities}}` once. CloudDO checks the token hash and expiry,
   spends it, records `host`, `epoch`, `wg_public_key`, `daemon`, and emits `cloud.machine.upsert`
   (revision = stream seq) plus an outbox item to TeamDO's peer map (lane 12). A restore or a re-bind
   raises `epoch` and needs a fresh token; a second bind with a spent token is refused.
3. `connect_info` (read, session or install; `cmux link` through the host credential relay): CloudDO
   checks the caller may reach the machine (team policy), derives `overlay_address` from the host id,
   reads `vpc_endpoint`/`gateway` from TeamDO's peer map (lane 12), and mints a `link_token` (signed,
   single host, single install, the allowed services, this epoch, at most 5 minutes). Not bound
   answers `cloud.machine.not_bound`; paused answers the record with `state: paused`.
4. Access (decision CLOUD-CONNECT-ACCESS, Cloud owner a9, 2026-10-04): on a team machine, team members
   may connect by default and an admin can restrict it; on a personal machine only the creator, plus
   grants the creator can revoke. Policy is per service (daemon, ssh). Agents act as their principal
   (their owner's access, never more). Every connect_info and every link_token mint is audited.
5. `cloud.machine.link_token {host, services}` (decision LINK-TOKEN-OP): install principals only, never
   a session (an app or page caller never holds a link token, decision LINK-RESOLVE); not a read and
   no idempotency key, because each call mints a fresh short-lived credential; risk execute; audited;
   never cached or logged. connect_info carries no credential.
6. Token format (decision LINK-TOKEN-FORMAT, a9, 2026-10-04): compact JWS, EdDSA (Ed25519) through
   Workers WebCrypto (no hand-written crypto). Header {alg: "EdDSA", kid, typ: "cmux-link+jwt"}; claims
   iss, aud = host id, sub = install id, svc (services), epoch, iat, exp (at most 300 s after iat), jti
   (128 random bits), team. Keyset {kid -> public key} goes to the VM at bind and is refreshed on an
   epoch raise or a rotation; at most 2 active kids. Private keys exist only in CloudDO's signing path:
   a Worker secret per environment (separate development, staging and production kids; KMS-backed
   later if required), never logged or cached. The VM daemon checks signature, aud, epoch, exp and
   refuses a jti it has seen within exp (replay). The backend lead owns the signer, the keyset
   publication and the shared vectors schemas/link-token/vectors.json (valid, expired, wrong aud,
   wrong epoch, replay, unknown kid, rotated kid). Target: with bind and connect_info, 2026-10-10.
7. Revocation: install revocation through UserDO closes links (the token is checked at hello and
   bound to one install); a machine delete emits `cloud.machine.removed` and drops the peer entry.
8. Files (decision FINDER-FS): Cloud files go only through the link `daemon` service, never a backend
   file route. Policy and grants may give `daemon` only together with `ssh` (team policy key
   cloud.connectServices and CloudDO grants share cloudServicesProblem; landed b3965540ce5).
   fs-v1 on Cloud depends on this backend work, now UNVERIFIED until it exists: bind records host,
   epoch, key and keyset; connect_info lists `daemon` for a caller the policy allows; link_token
   mints `daemon`. Verification (by 2026-10-10): on development, create a machine, bind it with the
   image's agent, call connect_info and link_token as an install, and have `cmux link` open `daemon`
   and list a directory; a caller without `ssh` gets no `daemon` in services and link_token refuses it.

## 6. Classic Cloud migration

Inputs: classic users and VMs live in classic Postgres `cmux-prod` (`cloud_vms`) and on the same
Freestyle account. Rule: no live connection from cmux-next to classic; a one-time read-only export
per wave.

1. Wave export (operator, read-only): for the users in the wave, export
   `{stack_user_id, billing_team_id, provider_vm_id, slug, display_name, size, status, created_at,
   snapshots}` as JSONL, encrypted, to a private R2 bucket `cmux-next-migration` (dev/staging/prod
   prefixes). Who runs it and with which read credential is Lawrence's call (the coordinator is
   asking). The export never contains secrets.
2. Import (backend admin op `cloud.migration.import {object}`, admin key only): for each user,
   map the Stack user id to the cmux-next user and personal team (same Stack project, so ids map
   1:1), write classic machines into that team's CloudDO as `classic: true` with the provider id
   allowlisted, and record the wave in MySQL `cloud_migrations`.
3. User move (`cloud.migration.start`): one way per user. The classic side gets the fence: a small
   classic main PR adds a read-only `moved_to_next` flag that makes classic show those machines
   as moved and refuse mutations; merged only with Lawrence's verdict. Until the fence is live for
   a user, the new backend shows imported machines read-only.
4. Upgrade (`cloud.machine.upgrade`): CloudDO installs the cmux-next daemon through Freestyle exec,
   binds it to the overlay and drops the classic badge; failure leaves a working classic machine.
5. Billing: same Stripe customer and subscription; the import links the existing Stripe customer
   to the team in `billing_customers`.
6. Verification per wave: counts of exported vs imported machines per user, a provider lookup of
   every imported id, and a report in MySQL. Rollback before the fence: delete the imported rows
   (no provider change). After the fence, rollback is a classic flag flip.

## 7. Order of work and target dates (updated 2026-10-04)

1. MySQL step 1 (done 2026-10-04, 019d7ff4d24) and step 2a (done 2026-10-04, e94afacf348): development
   Hyperdrive configs, copy and verify job, live dual write on development (verified: 13 tables equal).
2. MySQL step 2b: staging schema, staging Hyperdrive configs, one-time copy, dual write with hourly
   compare, target 2026-10-06; main schema by deploy request (id reported first, coordinator's go),
   target 2026-10-08; read switch per environment after 3 clean days.
3. CloudDO skeleton: ledger, Freestyle driver (port of TeamVmDO's), create/get/list/delete, quotas,
   per-environment keys and prefixes (cmuxnp-dev-, cmuxnp-stg-, cmuxnp-prod-), target 2026-10-08.
4. VM bind and `connect_info` (5.8), snapshots, start/pause/resize, events, MySQL projection, target
   2026-10-10; staging deploy for the Cloud v2 client, target 2026-10-12.
5. Shell relay, classic migration import, upgrade: after 2026-10-12.
6. (g) continues in parallel; g2 history goes to MySQL `automation_runs`.

## 8. Open decisions

- D1: staging Freestyle key: reuse cmux-next-dev with prefix `cmuxnp-stg-`, or a third key.
- D2: production prefix `cmuxnp-prod-` instead of the contract's `cmux-` (recommended, classic
  uses `cmux-`).
- D3: who runs the classic wave export and with which read-only credential.
- D4: search on MySQL FULLTEXT ngram now, accepting the unsharded-keyspace limit.

## 9. App-side state: UserDefaults audit (Q11, chrome-parity p01-process-151, 2026-10-27)

The rule of section 1 applies inside one Mac too: a fact lives where its single writer is. The
cmux-tui daemon is the authority for the session (chrome-parity p01-process.md section 3); the app
keeps only what draws its own windows. This is Chrome's split: per-window and per-install UI memory
is browser-process local state (`chrome/browser/prefs/browser_prefs.cc` registers it in Local State),
while what a profile decides (site permissions, protocol handlers) lives in the profile's
Preferences, which every window of that profile reads.

Query Q11: `rg -n 'UserDefaults' Packages/macOS/CmuxNext/Sources --glob '!**/*Tests*'` at
perf/chrome-parity 78715dfbd1f gives **181 hits in 92 files** (the plan counted 172 in 88 at an
older tip; the 9 new hits are the same kinds). Each hit has one class:

- **view** (stays in the app): layout of this Mac's windows, one-time hints and dismissals,
  recents, caches, a per-client "seen" cursor (the OSC 7501 contract makes "until seen" client
  view state), the identity, update and consent state of this install, reads of macOS or other
  apps' defaults, one-time imports into cmux-next.json.
- **shared** (moves out of the app's defaults): a fact about a daemon object (tab, chat,
  workspace), the ledger of a job whose work is daemon-side or must survive an app quit, a
  marker that guards a daemon-side write, a configuration value that decides what an agent's
  request does, or browser-profile data. "Moves to" says where: a daemon op or job, the
  browser profile (with Q11b, task 238), or a `cmux-next.json` setting.

Kinds: layout, hint, recents, cache, cursor, install, os, import, plumbing (a parameter or
injection; it follows its store) for view; session, job, setting, account, profile, moved, plumbing
for shared. A store whose file keeps both kinds of keys has the class of its shared key; its view
key stays when the store is split (the row says which).

Result: **141 view, 40 shared** (24 files). The app-side owner of a shared row is the
OWNERS.md row of its file; only the sidebar has one (a590). The daemon side of every move is
PL-daemon. Follow-ups, one per store (each a separate task, not done here):

1. Agent chat marks: `AgentPriorityMarks` (pinned, read, snoozed, archived) and the pinned chats
   of `SidebarChatHistory` (a590) become chat facts in the daemon, so the phone, the TUI and a
   relaunched app show the same marks.
2. Tab facts: `AgentTabs` outside adoptions and `ProxiedBrowserTabs` become tab fields in the
   daemon.
3. Home pins: `HomePinStore` keeps only rows the owner's `home-inbox` does not list; drop it when
   the inbox lists Cloud rows.
4. Daemon-side markers: `HomeDock` (dock takes tabs, placeholder check) and `UpdateFeedPosting`
   (posted version) move to the data they guard; the Feed post gets an idempotency key.
5. Jobs that must survive an app quit: the Cloud move ledger (`CloudMovedChats`,
   `CloudMoveRetries`, `CloudMoveSetup.ledger`, `DebugCloudMove`), Hosted Chief copy and create
   ledgers (`HostedChiefLedger`, `HomeCloudChiefs` created-here), and `CloudHostReplicaFollower`
   (published user, purge asked) become daemon jobs (the p01-process 239 pattern).
6. Settings outside cmux-next.json: `cloud.askBeforeAgentCreate`, Cloud move "Always" folders,
   `home.chief.choice`, the Chief views fleet URL, and the selected Cloud team
   (`cmux.auth.selectedTeamID`) become settings or daemon account state that the daemon reads.
7. Browser profile: `BrowserExternalApps` per-site schemes move with the profile (Q11b, task
   238), as Chrome keeps `protocol_handler.allowed_origin_protocol_pairs` in the profile
   (`chrome/browser/external_protocol/external_protocol_handler.cc`).
8. Done already: palette usage is a daemon store (`DaemonPaletteUsageStore`); the four
   `FrecencyPersistence` rows, `PaletteController` and `PaletteUsageLegacyHistory` are its fallback
   and legacy import. Delete them when every supported daemon has the store.

Line numbers are at 78715dfbd1f; re-run the query to find a row after the code moves.

<!-- q11: 181 hits, 92 files, 40 shared -->
| # | Hit (APP/...:line) | Class | Kind | What it keeps | Moves to | Follow-up owner |
|---|---|---|---|---|---|---|
| 1 | `CmuxNextAgentLanes/AgentPriorityMarks.swift:40` | shared | session | `cmux.next.sidebar.priorityMarks`: pinned, read, snoozed, archived marks on agent chats; acpmux already hides archived chats, the phone and the TUI should show the same marks | daemon op | Performance Lead (no OWNERS row) |
| 2 | `CmuxNextAgentLanes/AgentPriorityMarks.swift:44` | shared | session | `cmux.next.sidebar.priorityMarks`: pinned, read, snoozed, archived marks on agent chats; acpmux already hides archived chats, the phone and the TUI should show the same marks | daemon op | Performance Lead (no OWNERS row) |
| 3 | `CmuxNextAgentLanes/ChiefSidebar.swift:62` | view | layout | `sidebar.chief.showsIdleSubagents` toggle | stays | - |
| 4 | `CmuxNextAgentLanes/ChiefSidebar.swift:63` | view | layout | `sidebar.chief.showsIdleSubagents` toggle | stays | - |
| 5 | `CmuxNextAgentLanes/RemoteMachinesSidebar.swift:91` | view | layout | `sidebar.remoteMachines.expanded` rows | stays | - |
| 6 | `CmuxNextAgentLanes/RemoteMachinesSidebar.swift:92` | view | layout | `sidebar.remoteMachines.expanded` rows | stays | - |
| 7 | `CmuxNextApp/AgentTabs.swift:187` | shared | session | `cmux.agent-tabs.outside-adoptions`: outside chat to the tab that resumes it; the tab is a daemon object, so the CLI, TUI and a relaunched app must resolve the same tab | daemon op | Performance Lead (no OWNERS row) |
| 8 | `CmuxNextApp/AgentTabs.swift:239` | shared | session | `cmux.agent-tabs.outside-adoptions`: outside chat to the tab that resumes it; the tab is a daemon object, so the CLI, TUI and a relaunched app must resolve the same tab | daemon op | Performance Lead (no OWNERS row) |
| 9 | `CmuxNextApp/AppServices.swift:281` | view | plumbing | `rightSidebarDefaults` injected into RightSidebarModel | stays | - |
| 10 | `CmuxNextApp/BrowserHost/AppBrowserHost.swift:210` | view | install | `cmux.next.browserHost.installID` | stays | - |
| 11 | `CmuxNextApp/Cloud/CloudMoveAsks.swift:42` | shared | setting | `cmux.cloud.moveAlwaysFolders` (Always move this folder) decides moves an agent asks for; `cmux.cloud.moveUnfinishedDismissed` is view and stays when split | cmux-next.json | Performance Lead (no OWNERS row) |
| 12 | `CmuxNextApp/Cloud/CloudMoveSetup.swift:18` | shared | plumbing | `ledger` defaults of CloudMovedChats, CloudMoveAsks, CloudMoveRetries | with its store | Performance Lead (no OWNERS row) |
| 13 | `CmuxNextApp/Cloud/CloudMovedChats.swift:44` | shared | job | `cmux.cloud.movedChats`: ledger of Move to Cloud for Bring Back Local; belongs with the move job | daemon job | Performance Lead (no OWNERS row) |
| 14 | `CmuxNextApp/Cloud/CloudService.swift:375` | shared | setting | `cloud.askBeforeAgentCreate`: gates machine creates an agent asks for; a defaults-only switch outside cmux-next.json | cmux-next.json | Performance Lead (no OWNERS row) |
| 15 | `CmuxNextApp/CmuxApplication.swift:221` | view | os | `NSApplicationCrashOnExceptions` AppKit process flag | stays | - |
| 16 | `CmuxNextApp/Control/DebugCloudMove.swift:136` | shared | plumbing | debug seeding of the Cloud move ledger (follows CloudMovedChats) | with its store | Performance Lead (no OWNERS row) |
| 17 | `CmuxNextApp/Control/DebugCrashes.swift:98` | view | os | reads `NSApplicationCrashOnExceptions` | stays | - |
| 18 | `CmuxNextApp/Crash/CrashOnExceptions.swift:22` | view | os | registers `NSApplicationCrashOnExceptions` | stays | - |
| 19 | `CmuxNextApp/Crash/CrashReporter+Launch.swift:22` | view | install | crash-report consent read at launch | stays | - |
| 20 | `CmuxNextApp/Feed/UpdateFeedPosting.swift:15` | shared | job | `cmux.feed.updateCard.postedVersion` guards a write to the daemon's Feed; replace with an idempotency key on the post | daemon job | Performance Lead (no OWNERS row) |
| 21 | `CmuxNextApp/Feed/UpdateFeedPosting.swift:71` | shared | job | `cmux.feed.updateCard.postedVersion` guards a write to the daemon's Feed; replace with an idempotency key on the post | daemon job | Performance Lead (no OWNERS row) |
| 22 | `CmuxNextApp/HangReports/HangReportsController.swift:57` | view | install | `cmux.next.hangReports.asked`, `.consentedAt` (consent of this install) | stays | - |
| 23 | `CmuxNextApp/HangReports/HangReportsController.swift:80` | view | install | `cmux.next.hangReports.asked`, `.consentedAt` (consent of this install) | stays | - |
| 24 | `CmuxNextApp/HangReports/HangReportsController.swift:131` | view | install | `cmux.next.hangReports.asked`, `.consentedAt` (consent of this install) | stays | - |
| 25 | `CmuxNextApp/HangReports/HangReportsController.swift:135` | view | install | `cmux.next.hangReports.asked`, `.consentedAt` (consent of this install) | stays | - |
| 26 | `CmuxNextApp/HangReports/HangReportsController.swift:306` | view | install | `cmux.next.hangReports.asked`, `.consentedAt` (consent of this install) | stays | - |
| 27 | `CmuxNextApp/HangReports/HangReportsController.swift:321` | view | install | `cmux.next.hangReports.asked`, `.consentedAt` (consent of this install) | stays | - |
| 28 | `CmuxNextApp/HangReports/WatchdogReporting.swift:83` | view | install | hang-report consent read | stays | - |
| 29 | `CmuxNextApp/NewTab/NewTabChoiceMemory.swift:23` | view | recents | `newTab.lastAgent`, `.lastPicks`, `.harnessHintDismissed`, `.startedProject` | stays | - |
| 30 | `CmuxNextApp/NewTab/NewTabChoiceMemory.swift:28` | view | recents | `newTab.lastAgent`, `.lastPicks`, `.harnessHintDismissed`, `.startedProject` | stays | - |
| 31 | `CmuxNextApp/NewTabWeather.swift:7` | view | cache | `cmux.newTab.weather` last reading | stays | - |
| 32 | `CmuxNextApp/NewTabWeather.swift:30` | view | cache | `cmux.newTab.weather` last reading | stays | - |
| 33 | `CmuxNextApp/NewTabWeather.swift:81` | view | cache | `cmux.newTab.weather` last reading | stays | - |
| 34 | `CmuxNextApp/Notifications/NotificationCenterService+Lookup.swift:114` | view | cursor | `feed.handoff.since.<install>` watermark of this install | stays | - |
| 35 | `CmuxNextApp/Notifications/NotificationCenterService+Lookup.swift:115` | view | cursor | `feed.handoff.since.<install>` watermark of this install | stays | - |
| 36 | `CmuxNextApp/Notifications/NotificationCenterService+Lookup.swift:117` | view | cursor | `feed.handoff.since.<install>` watermark of this install | stays | - |
| 37 | `CmuxNextApp/Onboarding/BrowserImportOfferService.swift:24` | view | hint | browser import offer | stays | - |
| 38 | `CmuxNextApp/Onboarding/BrowserImportOfferService.swift:44` | view | hint | browser import offer | stays | - |
| 39 | `CmuxNextApp/Onboarding/BrowserImportOfferState.swift:16` | view | hint | `browserImportOffer.state` | stays | - |
| 40 | `CmuxNextApp/Onboarding/BrowserImportOfferState.swift:21` | view | hint | `browserImportOffer.state` | stays | - |
| 41 | `CmuxNextApp/PaletteUsage/DaemonPaletteUsageStore.swift:49` | shared | moved | palette usage is in the daemon already; the local store is the fallback for a daemon without it | daemon (done) | Performance Lead (no OWNERS row) |
| 42 | `CmuxNextApp/PaneFullWidthTabs.swift:13` | view | layout | `panes.fullWidthTab` | stays | - |
| 43 | `CmuxNextApp/PaneFullWidthTabs.swift:16` | view | layout | `panes.fullWidthTab` | stays | - |
| 44 | `CmuxNextApp/PaneTabBars.swift:14` | view | layout | `panes.tabBar` | stays | - |
| 45 | `CmuxNextApp/PaneTabBars.swift:18` | view | layout | `panes.tabBar` | stays | - |
| 46 | `CmuxNextApp/QuickComposer/QuickComposerController.swift:46` | view | layout | `quickComposer.floatingChatSession` (the floating window's chat) | stays | - |
| 47 | `CmuxNextApp/QuickComposer/QuickComposerController.swift:60` | view | layout | `quickComposer.floatingChatSession` (the floating window's chat) | stays | - |
| 48 | `CmuxNextApp/RightSidebar/RightSidebarModel.swift:26` | view | layout | `rightSidebar.width`, `.visible`, `.mode` | stays | - |
| 49 | `CmuxNextApp/RightSidebar/RightSidebarModel.swift:27` | view | layout | `rightSidebar.width`, `.visible`, `.mode` | stays | - |
| 50 | `CmuxNextApp/RightSidebar/RightSidebarModel.swift:30` | view | layout | `rightSidebar.width`, `.visible`, `.mode` | stays | - |
| 51 | `CmuxNextApp/ServerUpdates/ServerUpdatePresenter.swift:35` | view | hint | server notice snoozes (ServerUpdateSnoozeStore) | stays | - |
| 52 | `CmuxNextApp/SidebarSections/SidebarLayoutService.swift:65` | view | hint | `cmux.next.sidebar.recentsOffered` (the layout itself is a daemon op already) | stays | - |
| 53 | `CmuxNextApp/SidebarSections/SidebarLayoutService.swift:80` | view | hint | `cmux.next.sidebar.recentsOffered` (the layout itself is a daemon op already) | stays | - |
| 54 | `CmuxNextApp/UpdaterService+App.swift:44` | view | install | What's New record | stays | - |
| 55 | `CmuxNextApp/Viewers/CmuxPicker.swift:77` | view | hint | picker privacy explainer | stays | - |
| 56 | `CmuxNextApp/Viewers/ViewerRecents.swift:42` | view | recents | `viewer.recents.v1` | stays | - |
| 57 | `CmuxNextApp/Viewers/ViewerRecents.swift:47` | view | recents | `viewer.recents.v1` | stays | - |
| 58 | `CmuxNextBridge/ProgramStatusSeen.swift:104` | view | cursor | `cmux.next.programStatusSeen.v1` (OSC 7501 contract: until seen is client view state) | stays | - |
| 59 | `CmuxNextBridge/ProgramStatusSeen.swift:109` | view | cursor | `cmux.next.programStatusSeen.v1` (OSC 7501 contract: until seen is client view state) | stays | - |
| 60 | `CmuxNextBrowser/CEF/CEFDevToolsLayout.swift:101` | view | layout | `CmuxNextBrowser.devToolsLayout` | stays | - |
| 61 | `CmuxNextBrowser/CEF/CEFDevToolsLayout.swift:106` | view | layout | `CmuxNextBrowser.devToolsLayout` | stays | - |
| 62 | `CmuxNextBrowser/Core/BrowserExternalApps.swift:27` | shared | profile | `CmuxNextBrowser.externalSchemes`: per-site external app permission; Chrome keeps it in the profile (`protocol_handler.allowed_origin_protocol_pairs`, chrome/browser/external_protocol/external_protocol_handler.cc); moves with the profile (Q11b, task 238) | daemon (browser profile) | Performance Lead (no OWNERS row) |
| 63 | `CmuxNextBrowser/Core/BrowserExternalApps.swift:39` | shared | profile | `CmuxNextBrowser.externalSchemes`: per-site external app permission; Chrome keeps it in the profile (`protocol_handler.allowed_origin_protocol_pairs`, chrome/browser/external_protocol/external_protocol_handler.cc); moves with the profile (Q11b, task 238) | daemon (browser profile) | Performance Lead (no OWNERS row) |
| 64 | `CmuxNextBrowser/UI/BrowserFitFrameView.swift:20` | view | hint | `cmux.next.browser.fitExplained` | stays | - |
| 65 | `CmuxNextBrowser/UI/BrowserFitFrameView.swift:27` | view | hint | `cmux.next.browser.fitExplained` | stays | - |
| 66 | `CmuxNextBrowser/UI/BrowserFitFrameView.swift:81` | view | hint | `cmux.next.browser.fitExplained` | stays | - |
| 67 | `CmuxNextBrowser/UI/BrowserFitFrameView.swift:82` | view | hint | `cmux.next.browser.fitExplained` | stays | - |
| 68 | `CmuxNextBrowserImport/Parsers/SafariSearchEngineReader.swift:11` | view | os | reads Safari's `NSPreferredWebServices` in the global domain | stays | - |
| 69 | `CmuxNextBrowserImport/Sources/BrowserFolderGrants.swift:11` | view | install | security-scoped bookmarks of browser folders; only the app process that got the grant can resolve them | stays | - |
| 70 | `CmuxNextBrowserImport/Sources/BrowserFolderGrants.swift:12` | view | install | security-scoped bookmarks of browser folders; only the app process that got the grant can resolve them | stays | - |
| 71 | `CmuxNextBrowserImport/Sources/BrowserFolderGrants.swift:19` | view | install | security-scoped bookmarks of browser folders; only the app process that got the grant can resolve them | stays | - |
| 72 | `CmuxNextChiefPetWiring/ChiefMascotPicks.swift:21` | view | layout | `home.chief.mascots` appearance | stays | - |
| 73 | `CmuxNextChiefPetWiring/ChiefMascotPicks.swift:22` | view | layout | `home.chief.mascots` appearance | stays | - |
| 74 | `CmuxNextChiefPetWiring/ChiefMascotPicks.swift:29` | view | layout | `home.chief.mascots` appearance | stays | - |
| 75 | `CmuxNextChiefViews/ChiefViewsPageProvider.swift:44` | shared | setting | `cmuxNext.chiefViews.fleetURL`: a configured feed URL outside cmux-next.json | cmux-next.json | Performance Lead (no OWNERS row) |
| 76 | `CmuxNextCloud/Auth/CloudAuth.swift:66` | shared | account | `cmux.auth.selectedTeamID` decides the team the daemon's Cloud ops act for; `cmux.auth.hasTokens` and `.cachedUser` are caches and stay | daemon op | Performance Lead (no OWNERS row) |
| 77 | `CmuxNextCloud/Move/CloudMoveRetries.swift:62` | shared | job | `cmux.cloud.moveRetries`, `.bringBackRetries`: retries of Cloud moves | daemon job | Performance Lead (no OWNERS row) |
| 78 | `CmuxNextCrashReporting/CrashReportingPolicy.swift:39` | view | install | `sendAnonymousTelemetry` consent | stays | - |
| 79 | `CmuxNextCrashReporting/CrashReportingPolicy.swift:88` | view | install | `sendAnonymousTelemetry` consent | stays | - |
| 80 | `CmuxNextDaemon/Connection/PaletteUsageLegacyHistory.swift:4` | shared | moved | reads the legacy `cmuxNext.palette.frecency.v1` plists to import them into the daemon | daemon (done) | Performance Lead (no OWNERS row) |
| 81 | `CmuxNextDaemon/Launch/DaemonSocketMemory.swift:11` | view | cache | `cmuxNext.daemonSocket.<session>` launch cache | stays | - |
| 82 | `CmuxNextDaemon/Launch/DaemonSocketMemory.swift:13` | view | cache | `cmuxNext.daemonSocket.<session>` launch cache | stays | - |
| 83 | `CmuxNextDaemon/Launch/LaunchSnapshot.swift:127` | view | cache | `cmuxNext.launchSnapshotPath.<session>` launch cache | stays | - |
| 84 | `CmuxNextDaemon/Launch/LaunchSnapshot.swift:129` | view | cache | `cmuxNext.launchSnapshotPath.<session>` launch cache | stays | - |
| 85 | `CmuxNextDesign/WindowTitlebar.swift:19` | view | os | `AppleActionOnDoubleClick` | stays | - |
| 86 | `CmuxNextFiles/FolderViewMemory.swift:28` | view | layout | `files.folderViews` | stays | - |
| 87 | `CmuxNextFiles/FolderViewMemory.swift:33` | view | layout | `files.folderViews` | stays | - |
| 88 | `CmuxNextFiles/SidebarModel.swift:16` | view | layout | `files.sidebar.favorites`, `.visible` | stays | - |
| 89 | `CmuxNextFiles/SidebarModel.swift:26` | view | layout | `files.sidebar.favorites`, `.visible` | stays | - |
| 90 | `CmuxNextFloating/ChiefPet/ChiefPetPanel.swift:46` | view | layout | pet panel origin (ChiefPetPlacement) | stays | - |
| 91 | `CmuxNextFloating/ChiefPet/ChiefPetPlacement.swift:13` | view | layout | `chiefPet.origins` | stays | - |
| 92 | `CmuxNextFloating/FloatingChatFrames.swift:14` | view | layout | `quickComposer.floatingChatFrames` | stays | - |
| 93 | `CmuxNextFloating/FloatingChatFrames.swift:16` | view | layout | `quickComposer.floatingChatFrames` | stays | - |
| 94 | `CmuxNextHangReports/BlockingReportSink.swift:118` | view | install | `cmux.next.blockingReports.lastSent`, `.daemonLastSent` rate limits | stays | - |
| 95 | `CmuxNextHangReports/BlockingReportSink.swift:119` | view | install | `cmux.next.blockingReports.lastSent`, `.daemonLastSent` rate limits | stays | - |
| 96 | `CmuxNextHangReports/BlockingReportSink.swift:127` | view | install | `cmux.next.blockingReports.lastSent`, `.daemonLastSent` rate limits | stays | - |
| 97 | `CmuxNextHangReports/BlockingReportSink.swift:157` | view | install | `cmux.next.blockingReports.lastSent`, `.daemonLastSent` rate limits | stays | - |
| 98 | `CmuxNextHangReports/DaemonSiteLedger.swift:69` | view | install | `cmux.next.blockingReports.daemonSites` report dedupe | stays | - |
| 99 | `CmuxNextHangReports/DaemonSiteLedger.swift:74` | view | install | `cmux.next.blockingReports.daemonSites` report dedupe | stays | - |
| 100 | `CmuxNextHangReports/InstallIdentity.swift:11` | view | install | `cmux.next.hangReports.installId` | stays | - |
| 101 | `CmuxNextHangReports/InstallIdentity.swift:34` | view | install | `cmux.next.hangReports.installId` | stays | - |
| 102 | `CmuxNextHome/Sidebar/HomeSidebarWidth.swift:31` | view | layout | Home list width per window | stays | - |
| 103 | `CmuxNextHome/Sidebar/HomeSidebarWidth.swift:33` | view | layout | Home list width per window | stays | - |
| 104 | `CmuxNextHomeEngine/Home/HomeSwitchKeysSource.swift:16` | view | hint | `home.firstRun.switchKeysHidden` | stays | - |
| 105 | `CmuxNextHomeEngine/Home/HomeSwitchKeysSource.swift:22` | view | hint | `home.firstRun.switchKeysHidden` | stays | - |
| 106 | `CmuxNextHomeEngine/HomeChiefModelRecents.swift:4` | view | recents | `home.chief.recentModels` | stays | - |
| 107 | `CmuxNextHomeEngine/HomeChiefModelRecents.swift:10` | view | recents | `home.chief.recentModels` | stays | - |
| 108 | `CmuxNextHomeEngine/HomeChiefModelRecents.swift:11` | view | recents | `home.chief.recentModels` | stays | - |
| 109 | `CmuxNextHomeEngine/HomeChiefModelRecents.swift:15` | view | recents | `home.chief.recentModels` | stays | - |
| 110 | `CmuxNextHomeEngine/HomeDock.swift:67` | shared | job | `cmuxNext.home.dockTakesTabs`, `.placeholderShellChecked`: one-time markers of the daemon's pinned dock; the marker belongs with the dock | daemon job | Performance Lead (no OWNERS row) |
| 111 | `CmuxNextHomeEngine/HomeDock.swift:74` | shared | job | `cmuxNext.home.dockTakesTabs`, `.placeholderShellChecked`: one-time markers of the daemon's pinned dock; the marker belongs with the dock | daemon job | Performance Lead (no OWNERS row) |
| 112 | `CmuxNextHomeEngine/HomeSidebarSource.swift:15` | shared | session | `cmux.home.pins.<account>` and marks for rows the owner's `home-inbox` does not list yet; the owner keeps the rest | daemon op | Performance Lead (no OWNERS row) |
| 113 | `CmuxNextHomeEngine/HomeSidebarSource.swift:17` | shared | session | `cmux.home.pins.<account>` and marks for rows the owner's `home-inbox` does not list yet; the owner keeps the rest | daemon op | Performance Lead (no OWNERS row) |
| 114 | `CmuxNextHostedChief/HomeCloudChiefs+Tab.swift:124` | shared | job | Hosted Chiefs created here whose brain is not on yet, and creates sent without a reply; an app quit must not lose them | daemon job | Performance Lead (no OWNERS row) |
| 115 | `CmuxNextHostedChief/HomeCloudChiefs+Tab.swift:128` | shared | job | Hosted Chiefs created here whose brain is not on yet, and creates sent without a reply; an app quit must not lose them | daemon job | Performance Lead (no OWNERS row) |
| 116 | `CmuxNextHostedChief/HomeCloudChiefs+Tab.swift:130` | shared | job | Hosted Chiefs created here whose brain is not on yet, and creates sent without a reply; an app quit must not lose them | daemon job | Performance Lead (no OWNERS row) |
| 117 | `CmuxNextHostedChief/HostedChiefLedger.swift:11` | shared | job | `home.chief.hosted.*` copy steps into a Hosted Chief | daemon job | Performance Lead (no OWNERS row) |
| 118 | `CmuxNextHostedChief/HostedChiefModels.swift:107` | shared | setting | `home.chief.choice`: which Chief answers (hosted or this Mac) | cmux-next.json | Performance Lead (no OWNERS row) |
| 119 | `CmuxNextHostedChief/HostedChiefModels.swift:111` | shared | setting | `home.chief.choice`: which Chief answers (hosted or this Mac) | cmux-next.json | Performance Lead (no OWNERS row) |
| 120 | `CmuxNextImportCards/ImportSuggestionsCenter.swift:39` | view | hint | import suggestion cards | stays | - |
| 121 | `CmuxNextImportCards/ImportSuggestionsCenter.swift:49` | view | hint | import suggestion cards | stays | - |
| 122 | `CmuxNextImportSuggestions/ImportSuggestionState.swift:126` | view | hint | `importSuggestions.state` | stays | - |
| 123 | `CmuxNextImportSuggestions/ImportSuggestionState.swift:132` | view | hint | `importSuggestions.state` | stays | - |
| 124 | `CmuxNextOnboarding/System/BrowserFolderAccess.swift:16` | view | plumbing | BrowserFolderGrants store | stays | - |
| 125 | `CmuxNextPalette/FolderPicker/PickerPrivacy.swift:71` | view | hint | `picker.privacyExplainerShown` | stays | - |
| 126 | `CmuxNextPalette/FolderPicker/PickerPrivacy.swift:72` | view | hint | `picker.privacyExplainerShown` | stays | - |
| 127 | `CmuxNextPalette/FolderPicker/PickerPrivacy.swift:75` | view | hint | `picker.privacyExplainerShown` | stays | - |
| 128 | `CmuxNextPalette/FrecencyPersistence.swift:10` | shared | moved | `cmuxNext.palette.frecency.v1`: fallback of the daemon palette usage store | daemon (done) | Performance Lead (no OWNERS row) |
| 129 | `CmuxNextPalette/FrecencyPersistence.swift:11` | shared | moved | `cmuxNext.palette.frecency.v1`: fallback of the daemon palette usage store | daemon (done) | Performance Lead (no OWNERS row) |
| 130 | `CmuxNextPalette/FrecencyPersistence.swift:12` | shared | moved | `cmuxNext.palette.frecency.v1`: fallback of the daemon palette usage store | daemon (done) | Performance Lead (no OWNERS row) |
| 131 | `CmuxNextPalette/FrecencyPersistence.swift:15` | shared | moved | `cmuxNext.palette.frecency.v1`: fallback of the daemon palette usage store | daemon (done) | Performance Lead (no OWNERS row) |
| 132 | `CmuxNextPalette/PaletteController.swift:71` | shared | moved | frecency fallback (FrecencyPersistence) | daemon (done) | Performance Lead (no OWNERS row) |
| 133 | `CmuxNextPhoneAccess/CloudHostReplicaFollower.swift:52` | shared | job | `cmux.cloudReplica.publishedUser.v1`, `.purgeAskedDaemon.v1`: what the daemon published and was asked to purge | daemon job | Performance Lead (no OWNERS row) |
| 134 | `CmuxNextPhoneAccess/CloudHostReplicaFollower.swift:60` | shared | job | `cmux.cloudReplica.publishedUser.v1`, `.purgeAskedDaemon.v1`: what the daemon published and was asked to purge | daemon job | Performance Lead (no OWNERS row) |
| 135 | `CmuxNextPhoneAccess/PhoneAccessFollower.swift:69` | view | import | `cmux.phoneAccess.migrated.v1` one-time settings migration | stays | - |
| 136 | `CmuxNextRemoteLocalhostService/RemoteLocalhost/ProxiedBrowserTabs.swift:12` | shared | session | `cmux.next.browser.proxiedTabs`: tabs routed through the remote proxy; a tab fact | daemon op | Performance Lead (no OWNERS row) |
| 137 | `CmuxNextRemoteLocalhostService/RemoteLocalhost/ProxiedBrowserTabs.swift:29` | shared | session | `cmux.next.browser.proxiedTabs`: tabs routed through the remote proxy; a tab fact | daemon op | Performance Lead (no OWNERS row) |
| 138 | `CmuxNextRemoteLocalhostService/RemoteLocalhost/ProxiedBrowserTabs.swift:35` | shared | session | `cmux.next.browser.proxiedTabs`: tabs routed through the remote proxy; a tab fact | daemon op | Performance Lead (no OWNERS row) |
| 139 | `CmuxNextSettings/ClassicPreferenceImport.swift:4` | view | import | reads classic domains; `classicPreferencesImported` | stays | - |
| 140 | `CmuxNextSettings/ClassicPreferenceImport.swift:13` | view | import | reads classic domains; `classicPreferencesImported` | stays | - |
| 141 | `CmuxNextSettings/ClassicPreferenceImport.swift:35` | view | import | reads classic domains; `classicPreferencesImported` | stays | - |
| 142 | `CmuxNextSettings/ClassicPreferenceImport.swift:50` | view | import | reads classic domains; `classicPreferencesImported` | stays | - |
| 143 | `CmuxNextSettings/ClassicPreferenceImport.swift:52` | view | import | reads classic domains; `classicPreferencesImported` | stays | - |
| 144 | `CmuxNextSettings/ClassicPreferenceImport.swift:61` | view | import | reads classic domains; `classicPreferencesImported` | stays | - |
| 145 | `CmuxNextSettings/ClassicShortcutImport.swift:21` | view | import | reads classic domains; `classicShortcutsImported` | stays | - |
| 146 | `CmuxNextSettings/ClassicShortcutImport.swift:91` | view | import | reads classic domains; `classicShortcutsImported` | stays | - |
| 147 | `CmuxNextSettings/ClassicShortcutImport.swift:93` | view | import | reads classic domains; `classicShortcutsImported` | stays | - |
| 148 | `CmuxNextSettings/ClassicShortcutImport.swift:103` | view | import | reads classic domains; `classicShortcutsImported` | stays | - |
| 149 | `CmuxNextSettings/Managed/ManagedPreferences.swift:30` | view | os | MDM domain `com.manaflow.cmux` | stays | - |
| 150 | `CmuxNextSettings/RecentFonts.swift:11` | view | recents | `cmuxNext.recentFonts` | stays | - |
| 151 | `CmuxNextSettings/RecentFonts.swift:13` | view | recents | `cmuxNext.recentFonts` | stays | - |
| 152 | `CmuxNextSettings/SettingsDiagnosticToasts.swift:29` | view | hint | `settings.diagnosticNotices.announced` | stays | - |
| 153 | `CmuxNextSettings/SettingsDiagnosticToasts.swift:38` | view | hint | `settings.diagnosticNotices.announced` | stays | - |
| 154 | `CmuxNextSidebar/Model/SidebarChatHistory.swift:100` | shared | session | `cmux.next.sidebar.sessionTree.pinnedChats` (a pin is a chat fact); `.earlierFolded` is view and stays when split | daemon op | a590 |
| 155 | `CmuxNextSidebar/Model/SidebarChatHistory.swift:106` | shared | session | `cmux.next.sidebar.sessionTree.pinnedChats` (a pin is a chat fact); `.earlierFolded` is view and stays when split | daemon op | a590 |
| 156 | `CmuxNextSidebar/Model/SidebarCollapsedSections.swift:9` | view | layout | `cmux.next.sidebar.collapsedSections` | stays | - |
| 157 | `CmuxNextSidebar/Model/SidebarCollapsedSections.swift:11` | view | layout | `cmux.next.sidebar.collapsedSections` | stays | - |
| 158 | `CmuxNextSidebar/Model/SidebarLookedStore.swift:16` | view | cursor | `cmux.next.sidebar.lastLooked` (since-looked cursor of this Mac) | stays | - |
| 159 | `CmuxNextSidebar/Model/SidebarLookedStore.swift:28` | view | cursor | `cmux.next.sidebar.lastLooked` (since-looked cursor of this Mac) | stays | - |
| 160 | `CmuxNextSidebar/Model/SidebarSessionTree.swift:160` | view | layout | `sidebar.sessionTree.collapsed` | stays | - |
| 161 | `CmuxNextSidebar/Model/SidebarSessionTree.swift:162` | view | layout | `sidebar.sessionTree.collapsed` | stays | - |
| 162 | `CmuxNextTerminal/TerminalSurfaceView+Mouse.swift:144` | view | os | `com.apple.trackpad.forceClick` | stays | - |
| 163 | `CmuxNextUpdater/ManagedUpdatePolicy.swift:20` | view | os | managed `DisableAutoUpdate` | stays | - |
| 164 | `CmuxNextUpdater/ManagedUpdatePolicy.swift:27` | view | os | managed `DisableAutoUpdate` | stays | - |
| 165 | `CmuxNextUpdater/PulledBuildWatch.swift:76` | view | install | `cmux.next.updates.pulledBuildWaiting` | stays | - |
| 166 | `CmuxNextUpdater/PulledBuildWatch.swift:85` | view | install | `cmux.next.updates.pulledBuildWaiting` | stays | - |
| 167 | `CmuxNextUpdater/ServerUpdates/ServerUpdateSnoozeStore.swift:3` | view | hint | `cmux.serverUpdates.snoozedUntil` | stays | - |
| 168 | `CmuxNextUpdater/ServerUpdates/ServerUpdateSnoozeStore.swift:7` | view | hint | `cmux.serverUpdates.snoozedUntil` | stays | - |
| 169 | `CmuxNextUpdater/ServerUpdates/ServerUpdateSnoozeStore.swift:10` | view | hint | `cmux.serverUpdates.snoozedUntil` | stays | - |
| 170 | `CmuxNextUpdater/Tips/TipState.swift:3` | view | hint | `cmux.next.tips.*` | stays | - |
| 171 | `CmuxNextUpdater/Tips/TipState.swift:37` | view | hint | `cmux.next.tips.*` | stays | - |
| 172 | `CmuxNextUpdater/Tips/TipState.swift:52` | view | hint | `cmux.next.tips.*` | stays | - |
| 173 | `CmuxNextUpdater/UpdateHarness.swift:79` | view | install | update test harness | stays | - |
| 174 | `CmuxNextUpdater/UpdateSnooze.swift:5` | view | install | `cmux.next.updates.closedVersion`, `.remindAfter` | stays | - |
| 175 | `CmuxNextUpdater/UpdateSnooze.swift:31` | view | install | `cmux.next.updates.closedVersion`, `.remindAfter` | stays | - |
| 176 | `CmuxNextUpdater/UpdateSnooze.swift:36` | view | install | `cmux.next.updates.closedVersion`, `.remindAfter` | stays | - |
| 177 | `CmuxNextUpdater/UpdaterService.swift:185` | view | install | updater state of this install | stays | - |
| 178 | `CmuxNextUpdater/UpdaterService.swift:203` | view | install | updater state of this install | stays | - |
| 179 | `CmuxNextUpdater/WhatsNew/WhatsNewCenter.swift:56` | view | install | `cmux.next.whatsNew.lastSeenVersion` | stays | - |
| 180 | `CmuxNextUpdater/WhatsNew/WhatsNewTracker.swift:51` | view | install | `cmux.next.whatsNew.lastSeenVersion` | stays | - |
| 181 | `CmuxNextUpdater/WhatsNew/WhatsNewTracker.swift:56` | view | install | `cmux.next.whatsNew.lastSeenVersion` | stays | - |
