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

## 10. App-side state: file store audit (Q11b, chrome-parity p01-process-238, 2026-10-27)

This follows the UserDefaults audit (Q11, task 151, section 9) with the same rule: a fact lives
where its single writer is. The cmux-tui daemon is the authority for the session; the app keeps
only what draws its own windows and what belongs to this install. Chrome splits files the same
way: per-install state is browser-process Local State (`chrome/browser/prefs/browser_prefs.cc`,
the variations seed in `components/variations/variations_seed_store.cc`), while what a profile
keeps (history, downloads, site settings, zoom, cookies) lives in that profile's folder, which
every window of the profile reads (`chrome/browser/profiles/profile_impl.cc`).

Query Q11b: `rg -l 'applicationSupportDirectory|\.applicationSupport|Application Support'
Packages/macOS/CmuxNext/Sources --glob '!**/*Tests*'` at perf/chrome-parity 22726d00344 gives
**89 files with 249 hits** (the plan counted 85 files at an older tip; 83 of the hits are
`browser-sources.json`). One row per file, at its first hit. Each file has one class:

- **view** (stays in the app): a cache the app can rebuild or that only speeds its own first frame,
  a client outbox (prompts or drafts this app holds until their owner has them), this install's
  identity, update, feature and diagnostic files, reads of macOS or other apps' files, one-time
  imports, and path plumbing.
- **shared** (not the app's to keep): a fact about a daemon object (workspace, chat), browser
  profile data, a job ledger that guards a write another process owns, or a file another process
  already owns that the app only locates. "Moves to" names where it goes; "done" means another
  process is already the writer and the row stays as path code.

Kinds: cache, layout, outbox, install, diagnostics, os, external, import, job, plumbing for view;
profile, session, job, done, plumbing (a seam of a shared store; it follows that store) for shared.

Result: **55 view, 34 shared** (17 move, 13 done, 4 plumbing). The owner column is the OWNERS.md
row of the file ("Performance Lead (no OWNERS row)" otherwise); it owns the app side of a move.
"Moves to" names the receiving owner. Follow-ups, one per store (each a separate task, not done
here):

1. Browser profile store in the daemon: profile records (`BrowserProfileService`, a browser
   profile table in the home daemon's `profiles-v1`), page history (`BrowserVisitLog`, through
   `HistoryService`), ended downloads (`BrowserDownloadHistory`), site settings
   (`SiteSettingsRegistry`), zoom levels (`SiteZoomLevels`, both made by `BrowserSharedStores`),
   cookie backups (`CookieBackups`, one implementation with the headless host's
   `cmux-tui/crates/cmux-browser-host/src/cookie_backups.rs`) and imported browser data
   (`ImportedDataStore` in `OnboardingService`). Chrome keeps each of these in the profile:
   `components/history/core/browser/history_backend.cc` (history and downloads),
   `components/content_settings/core/browser/host_content_settings_map.cc`,
   `components/zoom/zoom_controller.cc` (HostZoomMap, `partition.per_host_zoom_levels`). Section 9
   follow-up 7 (`BrowserExternalApps`) moves with it. Then the CLI, the TUI's browser and the phone
   read the same profile as the app.
2. Bookmarks: `BookmarkService` already uses the home daemon's `bookmarks-v1`; delete its
   `bookmarks.json` fallback when every supported daemon serves the table.
3. Chromium data root: `CEFProfileStorage` (`<bundle id>/Chromium`) becomes the Chromium host
   process's own root, as `ChromiumHost` already is (p01-process host model, PL-cefhost).
4. Agent folders and uploads: acpmux makes the agent-home folder of a folderless workspace
   (`AgentHome`, read by `AgentTabs+AgentFolder`) and keeps local chat uploads (`AgentChatUploads`)
   through `chat.upload`, as Cloud chats do, so a chat started from the CLI, TUI or phone gets the
   same folder and files (PL-acpmux).
5. Workspace facts: each workspace's background and tint (`WorkspaceBackgroundStore`) become
   personal workspace rows in `profiles-v1`, where room and workspace themes already are; the
   daemon marks every incognito workspace ephemeral, torn-off tabs included, and
   `IncognitoWorkspaceLedger` is deleted.
6. Login environment: the daemon captures and keeps the allowlisted login environment
   (`LoginEnvironmentStore`), so a daemon the CLI or TUI starts gives terminals the same `PATH`.
7. Classic migration: the ledger of imported classic workspaces (`ClassicMigrationRepository`,
   written through `AppOnboardingServices+ClassicMigration`, read by `ImportSourceHelpers`) becomes
   an idempotency key on the daemon's workspace import.
8. Server fixes: `ServerFixLedger` merges into the privileged helper's `ServerFixPriorStore`, the
   writer of the fixes, so there is one ledger.
9. Done already: 13 rows locate files that the daemon, the link, `cmux server`, the Tasks owner, the
   Chromium host, the cmux-cua helper or the privileged helper write.

Not moved, on purpose: `TerminalThemeStore` (its own rule: per-terminal themes are per-client view
state, `profiles-v1` has no per-terminal column), `ImportSuggestionsCenter`'s job ledger (an import
reads other browsers' folders with the access the person gave this app, so the job runs here; its
output is profile data, follow-up 1), and the two outboxes (`AgentPanePromptJournal`,
`RecoveryDraftStore`), which hold what no other owner has yet.

Line numbers are at 22726d00344; re-run the query to find a row after the code moves.

<!-- q11b: 89 files, 249 hits, 34 shared -->
| # | File (APP/...:first hit) | Hits | Class | Kind | What it keeps | Moves to | Owner |
|---|---|---|---|---|---|---|---|
| 1 | `CmuxNextAgentPane/AgentHome.swift:6` | 4 | shared | session | `cmux/agent-home/<workspace id>`: cwd and only root of agent chats in a folderless workspace, made 0700 and marked so acpmux trusts it | acpmux makes it when it starts the chat (PL-acpmux) | Performance Lead (no OWNERS row) |
| 2 | `CmuxNextAgentPane/AgentPanePromptJournal.swift:38` | 2 | view | outbox | `<bundle id>/agent-pane-unsent-prompts.json`: prompts this app's pane sent that acpmux has not acknowledged | stays | Performance Lead (no OWNERS row) |
| 3 | `CmuxNextApp/AgentChatUploads.swift:10` | 2 | shared | session | `cmux/Chat Uploads/sessions/<session>/...`: files attached to a local chat, deleted with the chat | acpmux `chat.upload` (PL-acpmux) | Performance Lead (no OWNERS row) |
| 4 | `CmuxNextApp/AgentTabs+AgentFolder.swift:74` | 1 | shared | plumbing | reads `AgentHome.standard` once | with `AgentHome` | Performance Lead (no OWNERS row) |
| 5 | `CmuxNextApp/Bookmarks/BookmarkService.swift:16` | 1 | shared | profile | `<bundle id>/BrowserProfiles/bookmarks.json`: fallback when the home daemon does not serve `bookmarks-v1` | delete the fallback; `bookmarks-v1` is the store (PL-daemon) | Performance Lead (no OWNERS row) |
| 6 | `CmuxNextApp/BrowserDownloadHistory.swift:25` | 2 | shared | profile | `<bundle id>/downloads.json`: ended downloads of the Downloads list | daemon browser profile store (PL-daemon) | Performance Lead (no OWNERS row) |
| 7 | `CmuxNextApp/BrowserProfiles/BrowserProfileService.swift:17` | 3 | shared | profile | `<bundle id>/BrowserProfiles/profiles.json`: profile records and the new-tab profile | browser profile table in `profiles-v1` (PL-daemon) | Performance Lead (no OWNERS row) |
| 8 | `CmuxNextApp/ChromiumRuntimeService.swift:86` | 1 | view | plumbing | passes Application Support to `ChromiumRuntimeStore` | stays | Performance Lead (no OWNERS row) |
| 9 | `CmuxNextApp/Crash/AppRunMarker.swift:123` | 2 | view | diagnostics | `cmux-next/<bundle id>/`: run marker of this app process (unclean exit) | stays | Performance Lead (no OWNERS row) |
| 10 | `CmuxNextApp/Crash/CrashReporter+Launch.swift:224` | 1 | shared | done | locates the daemon's `cmux-tui/sessions` folder for owner crash reports | done (daemon) | Performance Lead (no OWNERS row) |
| 11 | `CmuxNextApp/ErrorJournal/ErrorJournalService.swift:14` | 3 | view | diagnostics | `<bundle id>/errors`: this build's error journal | stays | Performance Lead (no OWNERS row) |
| 12 | `CmuxNextApp/Handlers/SettingsHandlers.swift:77` | 1 | view | external | doc comment: Ghostty's config in Application Support | stays | Performance Lead (no OWNERS row) |
| 13 | `CmuxNextApp/History/HistoryService.swift:15` | 1 | shared | plumbing | passes `<bundle id>` to each profile's `BrowserVisitLog` | with `BrowserVisitLog` | Performance Lead (no OWNERS row) |
| 14 | `CmuxNextApp/Home/HomeService+ChiefMigration.swift:36` | 2 | view | import | one-time read of old Chiefs' folders | stays | Performance Lead (no OWNERS row) |
| 15 | `CmuxNextApp/Onboarding/AppOnboardingServices+ClassicMigration.swift:32` | 3 | shared | job | `cmux-next/<bundle id>/classic-migration`: ledger of classic workspaces imported into the daemon session, and backups | idempotency key on the daemon's import (PL-daemon) | Performance Lead (no OWNERS row) |
| 16 | `CmuxNextApp/Onboarding/OnboardingService.swift:43` | 3 | shared | profile | `<bundle id>/BrowserImport` (`ImportedDataStore`: imported history and bookmarks per source profile); classic migration folder seam | daemon browser profile store (PL-daemon) | Performance Lead (no OWNERS row) |
| 17 | `CmuxNextApp/Pages/Files/TableWidths/MarkdownTableWidthRepository.swift:6` | 3 | view | cache | `<bundle id>/MarkdownTableWidths`: copy of a file's `com.cmux.markdown.tables` attribute for volumes without attributes | stays | PL-webui |
| 18 | `CmuxNextApp/Permissions/SystemPermissionProbe.swift:98` | 1 | view | os | opens `com.apple.TCC/TCC.db` to probe Full Disk Access | stays | Performance Lead (no OWNERS row) |
| 19 | `CmuxNextApp/Server/LocalServerSource.swift:105` | 1 | shared | done | watches `cmux/server/roles/status.json`, written by `cmux server` | done (`cmux server`) | Performance Lead (no OWNERS row) |
| 20 | `CmuxNextApp/Server/ServerFixLedger.swift:38` | 3 | shared | job | `cmux/server-fix-ledger/<helper label>.json`: system fixes the privileged helper applied, kept for revert | the helper's `ServerFixPriorStore` (Performance Lead) | Performance Lead (no OWNERS row) |
| 21 | `CmuxNextApp/Server/ServerReachPlan.swift:115` | 2 | shared | done | locates the link's `link/config.json` (`cmux link init` writes it) | done (link) | Performance Lead (no OWNERS row) |
| 22 | `CmuxNextApp/Settings/DebugSettingsService.swift:136` | 2 | view | install | `cmux/<tag or builds/key>/debug-tunables.json` | stays | Performance Lead (no OWNERS row) |
| 23 | `CmuxNextApp/SidebarSnapshot/SidebarSnapshotFile.swift:39` | 1 | view | cache | sidebar snapshot for the first frame before the daemon answers; folder of small per-build files | stays | Performance Lead (no OWNERS row) |
| 24 | `CmuxNextApp/Tasks/TasksPageService.swift:14` | 1 | shared | done | doc comment: the Tasks owner's socket | done (Tasks owner) | Performance Lead (no OWNERS row) |
| 25 | `CmuxNextApp/Themes/TerminalThemeStore.swift:8` | 2 | view | layout | `<bundle id>/terminal-themes.json`: per-terminal themes, per-client by the file's own rule | stays | Performance Lead (no OWNERS row) |
| 26 | `CmuxNextApp/Themes/WorkspaceBackgroundStore.swift:9` | 2 | shared | session | `<bundle id>/workspace-backgrounds.json` and `workspace-background-opacity.json`: each workspace's background and tint | personal workspace rows in `profiles-v1` (PL-daemon) | Performance Lead (no OWNERS row) |
| 27 | `CmuxNextApp/Windows/IncognitoWorkspaceLedger.swift:49` | 2 | shared | session | `<bundle id>/incognito-workspaces.json`: incognito workspaces the daemon does not know as ephemeral (torn-off tabs) | daemon ephemeral flag, then delete (PL-daemon) | Performance Lead (no OWNERS row) |
| 28 | `CmuxNextBrowser/CEF/CEFNativeMessaging.swift:7` | 4 | view | external | native messaging host folders (cmux's Chromium root, Chromium's and Chrome's) | stays | PL-cefhost |
| 29 | `CmuxNextBrowser/CEF/CEFProfileStorage.swift:6` | 2 | shared | profile | `<bundle id>/Chromium`: Chromium's `root_cache_path` and each `Profile-<uuid>` | the Chromium host process's own root (PL-cefhost) | PL-cefhost |
| 30 | `CmuxNextBrowser/CEF/ChromiumHost/ChromiumHostLaneSpawner.swift:20` | 1 | shared | done | `<bundle id>/ChromiumHost`: the host process's root | done (Chromium host) | PL-cefhost |
| 31 | `CmuxNextBrowser/CEF/ChromiumHost/ChromiumHostPrototype.swift:83` | 2 | shared | done | `<bundle id>/ChromiumHost`: the host process's root | done (Chromium host) | PL-cefhost |
| 32 | `CmuxNextBrowser/CEF/ChromiumUseRecord.swift:5` | 1 | view | cache | when this app last opened a Chromium tab (launch warmup decision) | stays | PL-cefhost |
| 33 | `CmuxNextBrowser/Core/BrowserSharedStores.swift:65` | 1 | shared | plumbing | makes the site settings and zoom stores | with `SiteSettingsRegistry`, `SiteZoomLevels` | Performance Lead (no OWNERS row) |
| 34 | `CmuxNextBrowser/Core/SiteZoomLevels.swift:21` | 2 | shared | profile | `<bundle id>/SiteZoom/<profile>.json`: per-site zoom | daemon browser profile store (PL-daemon) | Performance Lead (no OWNERS row) |
| 35 | `CmuxNextBrowserAutomation/CookieBackups.swift:12` | 3 | shared | profile | `<bundle id>/CookieBackups/<id>.bin`: encrypted cookie backups for automation restore | daemon browser profile store, one implementation with the headless host (PL-daemon) | Performance Lead (no OWNERS row) |
| 36 | `CmuxNextBrowserCore/Core/BrowserDownloadTempFiles.swift:19` | 2 | view | job | `<bundle id>/download-temp-files.json`: `.cmuxdownload` files this process writes, for crash cleanup | stays with the download writer | Performance Lead (no OWNERS row) |
| 37 | `CmuxNextBrowserCore/PageInfo/Model/SiteSettingsRegistry.swift:6` | 3 | shared | profile | `<bundle id>/SiteSettings/<profile>.json`: site permissions both engines share | daemon browser profile store (PL-daemon) | Performance Lead (no OWNERS row) |
| 38 | `CmuxNextBrowserImport/Resources/browser-sources.json:54` | 83 | view | external | other browsers' data folders, for import | stays | Performance Lead (no OWNERS row) |
| 39 | `CmuxNextBrowserImport/Sources/ImportBrowser.swift:67` | 1 | view | external | default data folder of another browser | stays | Performance Lead (no OWNERS row) |
| 40 | `CmuxNextChromiumRuntime/ChromiumRuntimeStore.swift:10` | 2 | view | install | `<bundle id>/ChromiumRuntime`: downloaded Chromium runtime packs | stays | Performance Lead (no OWNERS row) |
| 41 | `CmuxNextCloud/Config/CloudConfiguration.swift:59` | 1 | view | install | `cmux/<bundle id>`: the old app's file-fallback token folder | stays | Performance Lead (no OWNERS row) |
| 42 | `CmuxNextCloud/Tunnel/CloudPaths.swift:16` | 1 | view | install | `cmux/<bundle id>/cloud-next`: WireGuard key, device id and link client state of this install | stays | Performance Lead (no OWNERS row) |
| 43 | `CmuxNextCodeRouter/Detection/ProviderDetector+ClaudeConfigured.swift:13` | 1 | view | external | Claude Code's managed settings | stays | Performance Lead (no OWNERS row) |
| 44 | `CmuxNextCompat/BuildIdentity+AppNames.swift:102` | 1 | view | plumbing | path of a build's daemon state folder | stays | Performance Lead (no OWNERS row) |
| 45 | `CmuxNextCompat/BuildIdentity.swift:169` | 1 | view | plumbing | path of a build's support folder | stays | Performance Lead (no OWNERS row) |
| 46 | `CmuxNextCompat/Threading/CheckedFileManager.swift:229` | 1 | view | plumbing | doc comment of the checked `FileManager.url` | stays | Performance Lead (no OWNERS row) |
| 47 | `CmuxNextComputerUse/ComputerUseHelperDaemon.swift:294` | 1 | shared | done | `cmux/cmux-cua/runtime/<scope>/state`: the cmux-cua helper's state | done (cmux-cua helper) | Performance Lead (no OWNERS row) |
| 48 | `CmuxNextDaemon/Launch/CloudLinkLauncher.swift:75` | 2 | shared | done | the daemon state root the link gets as `--state-dir` | done (daemon) | Performance Lead (no OWNERS row) |
| 49 | `CmuxNextDaemon/Launch/DaemonLauncher+Build.swift:14` | 1 | shared | done | a build's daemon session and state folder | done (daemon) | Performance Lead (no OWNERS row) |
| 50 | `CmuxNextDaemon/Launch/DaemonLauncher.swift:156` | 7 | shared | done | `cmux/tags/<tag>/tui`: daemon state folders | done (daemon) | Performance Lead (no OWNERS row) |
| 51 | `CmuxNextDaemon/Launch/LastAppCLIPointer.swift:6` | 2 | view | install | `cmux/last-app-cli`: the CLI of the app opened last, read by the dev shim | stays | Performance Lead (no OWNERS row) |
| 52 | `CmuxNextDaemon/Launch/LegacyChannelSession.swift:8` | 3 | shared | done | picks the daemon's legacy state root for old channel sessions | done (daemon) | Performance Lead (no OWNERS row) |
| 53 | `CmuxNextDaemon/Launch/LoginEnvironmentStore.swift:43` | 3 | shared | session | `cmux/login-environment.json`: allowlisted login environment given to the daemon and its terminals | daemon captures and keeps it (PL-daemon) | Performance Lead (no OWNERS row) |
| 54 | `CmuxNextDesign/AppleWallpapers.swift:19` | 1 | view | os | macOS aerials folder | stays | Performance Lead (no OWNERS row) |
| 55 | `CmuxNextDesign/BackdropArtDownloads.swift:8` | 3 | view | cache | `<bundle id>/Backgrounds`: downloaded art, named by SHA-256 | stays | Performance Lead (no OWNERS row) |
| 56 | `CmuxNextDesignTokens/Quit/RecoveryDraftStore.swift:120` | 2 | view | outbox | `cmux/recovery`: unsaved drafts of this app's editors | stays | Performance Lead (no OWNERS row) |
| 57 | `CmuxNextFeatures/FeatureRemoteStore.swift:9` | 2 | view | install | `<app folder>/features`: signed flags document, latest and last good | stays | PL-perf-lab |
| 58 | `CmuxNextFeatures/FeatureSafeMode.swift:159` | 1 | view | install | crash streak file in `features` | stays | PL-perf-lab |
| 59 | `CmuxNextFeatures/LowEntropySource.swift:79` | 1 | view | install | low-entropy value in `features` (Chrome: `components/metrics/entropy_state.cc`) | stays | PL-perf-lab |
| 60 | `CmuxNextHangReports/HangReportStore.swift:4` | 1 | view | diagnostics | `<build>/HangReports` | stays | PL-telemetry |
| 61 | `CmuxNextHistory/BrowserVisitLog.swift:51` | 1 | shared | profile | `<bundle id>/BrowserProfiles/<profile>/History.sqlite`: page history | daemon browser profile store (PL-daemon) | Performance Lead (no OWNERS row) |
| 62 | `CmuxNextImportCards/ImportSourceHelpers.swift:15` | 1 | shared | plumbing | reads the classic migration ledger to count what is left | with the classic migration ledger | Performance Lead (no OWNERS row) |
| 63 | `CmuxNextImportCards/ImportSuggestionsCenter.swift:56` | 2 | view | job | `cmux-next/<bundle id>/import-jobs.json`: import jobs saved after each step for Resume | stays (reads with this app's folder access) | Performance Lead (no OWNERS row) |
| 64 | `CmuxNextImportSuggestions/ImportScanner.swift:189` | 1 | view | external | Ghostty's config, to list it as in use | stays | Performance Lead (no OWNERS row) |
| 65 | `CmuxNextMacScreen/MacScreenStore.swift:65` | 2 | view | cache | `<bundle id>/mac-screen.json`: this Mac's name as registered with the device hub (the authority) and devices' names as last seen | stays | Performance Lead (no OWNERS row) |
| 66 | `CmuxNextMobile/MobileHost/MobileHostDeviceIdentity.swift:4` | 1 | view | install | `device-id` of this Mac for the phone service | stays | Performance Lead (no OWNERS row) |
| 67 | `CmuxNextMobile/MobileHost/MobileHostStateDirectory.swift:3` | 2 | view | install | `<bundle id>/cmux-mobile-host`: device id and paired phones, written by the phone host this app runs | stays | Performance Lead (no OWNERS row) |
| 68 | `CmuxNextMobileHost/IPhoneHostService+Wiring.swift:62` | 1 | view | plumbing | passes the state folder to the phone host | stays | Performance Lead (no OWNERS row) |
| 69 | `CmuxNextOnboarding/Model/OnboardingStrings+Access.swift:25` | 1 | view | external | comment: Chrome's folder in the access step | stays | Performance Lead (no OWNERS row) |
| 70 | `CmuxNextOnboarding/Steps/ImportAccessView.swift:68` | 1 | view | external | display path of a browser folder | stays | Performance Lead (no OWNERS row) |
| 71 | `CmuxNextOnboarding/System/AgentProjectScan.swift:41` | 1 | view | external | onboarding scan of agent homes (cmux's agent-home among them) | stays | Performance Lead (no OWNERS row) |
| 72 | `CmuxNextOnboarding/System/ClassicMigrationRepository.swift:22` | 1 | shared | job | the classic migration ledger and backups | idempotency key on the daemon's import (PL-daemon) | Performance Lead (no OWNERS row) |
| 73 | `CmuxNextOnboarding/System/ClassicMigrationSourceFinder.swift:5` | 2 | view | import | finds classic's session file | stays | Performance Lead (no OWNERS row) |
| 74 | `CmuxNextOnboarding/System/ClassicSessionImport.swift:52` | 2 | view | import | reads classic's newest snapshot | stays | Performance Lead (no OWNERS row) |
| 75 | `CmuxNextPages/WebDevOverride.swift:9` | 1 | view | install | doc example: the developer's web overlay folder | stays | PL-webui |
| 76 | `CmuxNextPhoneAccess/MobileHostService.swift:190` | 1 | view | install | the phone host's device keys and cache | stays | Performance Lead (no OWNERS row) |
| 77 | `CmuxNextRemote/SSH/SSHMachineLink.swift:17` | 1 | view | install | `cmux/<bundle id>/ssh-next`: SSH client identity, known daemons, download cache | stays | Performance Lead (no OWNERS row) |
| 78 | `CmuxNextScreenshots/ScreenshotTool.swift:69` | 3 | view | external | screenshot apps' settings and folders | stays | Performance Lead (no OWNERS row) |
| 79 | `CmuxNextServerHelper/ServerFixPriorStore.swift:35` | 2 | shared | done | `/Library/Application Support/cmux/server-helper/<label>.json`, written by the privileged helper | done (privileged helper) | Performance Lead (no OWNERS row) |
| 80 | `CmuxNextSettings/EditorImport/EditorSettingsSources.swift:37` | 1 | view | external | other editors' settings files | stays | Performance Lead (no OWNERS row) |
| 81 | `CmuxNextSettings/EditorImport/ImportableEditor.swift:4` | 4 | view | external | doc comments: other editors' settings folders | stays | Performance Lead (no OWNERS row) |
| 82 | `CmuxNextSettings/Managed/ManagedStatusReport.swift:69` | 1 | view | install | `cmux/managed-status*.json`: the MDM status report | stays | Performance Lead (no OWNERS row) |
| 83 | `CmuxNextTasks/SocketTasksSource.swift:54` | 2 | shared | done | `cmux/tasks/<team>/tasks.sock`: the Tasks owner's socket | done (Tasks owner) | Performance Lead (no OWNERS row) |
| 84 | `CmuxNextTerminal/GhosttyRuntime+ConfigFiles.swift:37` | 3 | view | external | Ghostty's config files | stays | PL-ghostty |
| 85 | `CmuxNextTerminal/GhosttyRuntime+DefaultFiles.swift:6` | 9 | view | external | classic cmux's Ghostty layer, `<bundle id>/config.ghostty` | stays | PL-ghostty |
| 86 | `CmuxNextUpdater/UpdaterService+Rollback.swift:35` | 1 | view | install | `cmux-next/<bundle id>/versions`: kept previous versions | stays | Performance Lead (no OWNERS row) |
| 87 | `CmuxNextUpdater/WebUpdates/WebUpdateStore.swift:6` | 2 | view | install | `<bundle id>/web-updates`: web updates to serve | stays | Performance Lead (no OWNERS row) |
| 88 | `CmuxNextUpdater/WhatsNew/WhatsNewLastUpdate.swift:38` | 3 | view | install | `cmux-next/<bundle id>/last-update.json`: the update record for What's New | stays | Performance Lead (no OWNERS row) |
| 89 | `CmuxNextWakeups/FolderChanges.swift:9` | 1 | view | plumbing | doc comment: stand-in watch through Application Support | stays | Performance Lead |
