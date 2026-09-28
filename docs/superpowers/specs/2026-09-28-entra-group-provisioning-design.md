# Entra ID Group-Driven User Provisioning — Design

**Date:** 2026-09-28
**Status:** Draft for owner review. Nothing is implemented yet.
**Issue:** #73. Related: #1 (mailto onboarding at keygen, shipped), #64 (Bing grounding Entra auth)

## 1. Goal

Today a router API user exists because an operator ran `modelrouter user create`
(or `POST /admin/users`). Someone then issues keys and attaches budget rules by
hand. When that person leaves the team, their keys stay live until an operator
remembers to disable them.

This design lets an operator say **"members of Entra group X are router users
with permission set Y"**, and have the router keep that true:

- a new member of a mapped group becomes a router user, and can get a key;
- a member's permissions come from the groups they belong to;
- a member who leaves every mapped group has their keys **disabled right away**,
  the same way `modelrouter key disable` works.

Hard constraints (from the issue):

- **Fully optional.** The router runs outside Azure. With the feature off
  (the default), no code path changes, no Graph call is made, and no new
  config is required.
- **Never grant on a Graph error.** A failed or ambiguous directory read can
  only leave state as it is, or take access away. It can never add access.
- **Manually provisioned users keep working** with no migration step.

Out of scope: SCIM inbound provisioning (see §13, alternatives), non-Entra
directories (the model below is written so an LDAP or Google Workspace source
could plug in later, but only Entra is designed here), and user self-service
key retrieval.

## 2. What exists today (the footholds, as they are in the code)

| Piece | Where | What it gives us | Gap |
|---|---|---|---|
| Admin OIDC login | `src/api/admin/oidc.rs`, `OidcConfig` in `src/config/schema.rs`, `migrations/010_admin_oidc.sql` | Authorization-code + PKCE login. ID-token validation against JWKS. `admin_users.oidc_subject`. Auto-provisioning with a single fixed `auto_provision_role`, checked against `ADMIN_ROLE_VOCABULARY`. | Covers **admin sessions only**. The role is set once at first login and never re-evaluated. There is no groups claim and no link to API `users`. |
| Entra credential sources | `src/providers/azure_credentials.rs` (`AzureSettings`, `AzureCredential`, managed identity / workload identity / client secret / CLI / `default`), `src/providers/azure_entra.rs` (`TokenProvider`, scope helpers), `src/providers/credentials.rs` (`CredentialChain`, typed `credential_expired`, health verdicts) | Hand-rolled token acquisition with caching, expiry skew, reauth classification, and `/health/deep` reporting. | The issue mentions `bing_grounding/auth.rs`. That file is gone: commit `5e56ff4f` already promoted the logic into a shared module. The one thing still tied to providers is that `AzureSettings::from_config` takes a `ProviderConfig` (see §6). |
| Router users and keys | `users` (`012`, `014`), `api_keys` (`007`, `011`, `013`) | `UserRepository::set_enabled`, `ApiKeyRepository::disable_key` (sets `disabled_at`), `disable_all_keys_for_user`. `AuthenticatedUser` reads the key and the user from the DB **on every request** (`src/api/auth.rs`), with no cache. | No external identity column on `users`. |
| Router groups | `groups`, `group_memberships` (`014`, `020`), `GroupRepository` | Groups with priority, and time-bounded memberships (`joined_at` / `disabled_at`). They drive cost attribution in reports. | Groups **do not gate anything**. `PolicyEngine::check` (`src/router/policy.rs`) enforces key, user, project and global budget rules. `budget_rules.group_name` is shown on the budgets page but never enforced. `PolicyConditionConfig` matches on `tag`, `user_id` and `model` only. |
| Declarative policy | `[[policy_rules]]`, `src/router/declarative_policy.rs` | Config-driven allow-lists, USD budgets and cache opt-out. Hot-reloaded through `ArcSwap<Settings>`. | No group condition. |
| Keygen onboarding (#1) | `src/api/admin/dashboard.rs` mailto button | A `mailto:` to `users.email` with the new key. | Needs an admin to click it. Nothing sends mail from the server. |
| Background jobs | `src/cli/mod.rs` `serve` (a 60 s experiment sweeper, a 5 min affinity sweeper, archival) | An established `tokio::spawn` + `interval` pattern, where failures are logged and never fatal. | — |

The real gap is not "no Graph client". It is **"groups carry no permissions"**.
Mapping Entra groups onto router groups only helps once router groups gate
something. That work is Phase 1 below, and it is useful even with Entra off.

## 3. Permission model: Entra group → router group → permissions

Two layers. Each does one job:

```
Entra group (object id)  ──sync──▶  router group (groups row, managed_by = 'entra')
                                         │
                                         ├─ [[policy_rules]] with condition.group = "<router group>"
                                         ├─ budget_rules.group_name = "<router group>"   (now enforced)
                                         ├─ key template (project / label / session window)
                                         └─ optional admin role (viewer | superadmin)
```

**Why an extra layer instead of hanging permissions directly on Entra group
ids:**

1. Permissions keep working with Entra off. A manually managed router group
   gets exactly the same enforcement, so there is one enforcement path, not two.
2. Reports already attribute cost by router group (`group_memberships`
   join, priority wins), so Entra-driven users show up in existing reports with
   no change.
3. The directory is only a **membership source**. Swapping or adding a source
   later touches the sync module, not the policy engine.

### 3.1 What a group can grant (answers issue question 1)

"All of the above", through mechanisms that already exist:

| Permission | Mechanism | New work |
|---|---|---|
| Model allow-list, USD budget, cache opt-out | `[[policy_rules]]` + new `condition.group` | Add `group: Option<String>` to `PolicyConditionConfig`, and load the user's active group names into `User` in the auth extractor (one indexed query). |
| Budget / rate / token / concurrency limits | `budget_rules.group_name` | Enforce group rules in `PolicyEngine::check` between the user/key rules and the project rules. Keep skipping `window = "target"` rules, which stay informational as they are today. |
| API key issuance | Key template on the mapping (`project`, `label`, `session_window_secs`, `expires_in_days`) | See §5. |
| Admin dashboard role | `admin_role` on the mapping, checked against `ADMIN_ROLE_VOCABULARY` | See §3.4. |

**Multiple groups.** A user in several mapped groups is a member of each router
group. Enforcement rules:

- Declarative rules: `find_matching_rule` already picks the highest-priority
  matching rule, so `condition.group` rules compose the way rules do today.
- Group budget rules: **every** applicable group rule is checked, and the most
  restrictive one wins. This matches how user and key rules already stack
  (`min_concurrent` is the minimum across rules). A user never gets a higher
  limit from being in more groups. That is the least-privilege reading, and it
  is an open question (§12, Q2).

### 3.2 Config shape (answers issue question 4)

The mapping lives in `config.toml`, not the DB. Access policy is then
reviewable, diffable, and hot-reloaded like `[[policy_rules]]`. The DB records
**state** (who is currently a member, and since when), never **policy**.

```toml
[directory.entra]
enabled = false                         # default; the whole feature is inert when false
tenant_id = "00000000-0000-0000-0000-000000000000"
# Same vocabulary and precedence as the Azure providers' credential_source:
# default | managed-identity | workload-identity | client-secret | cli
credential_source = "managed-identity"
client_id = ""                          # user-assigned MI / app registration; falls back to AZURE_CLIENT_ID
client_secret = ""                      # client-secret only; prefer the AZURE_CLIENT_SECRET env var
graph_host = "https://graph.microsoft.com"   # sovereign clouds: graph.microsoft.us, microsoftgraph.chinacloudapi.cn
sync_interval_secs = 300
membership = "transitive"               # "direct" | "transitive" (nested groups), see §4.3
# Safety rails (§7)
max_disable_fraction = 0.25             # refuse a pass that would disable more than 25% of managed users
stale_after_secs = 86400                # health goes "degraded" after this long with no good sync
on_stale = "freeze"                     # "freeze" | "disable_managed" (§7.3)

[[directory.entra.group]]
entra_group_id = "11111111-1111-1111-1111-111111111111"   # object id; display names are not unique and can be renamed
router_group = "research"
priority = 10
admin_role = ""                         # "" = none; else one of ADMIN_ROLE_VOCABULARY
[directory.entra.group.key]             # optional: issue a key on provisioning (§5)
project = "research"
label = "entra"
session_window_secs = 1800
expires_in_days = 90

[[directory.entra.group]]
entra_group_id = "22222222-2222-2222-2222-222222222222"
router_group = "platform-admins"
admin_role = "superadmin"

[[policy_rules]]
name = "research-models"
condition = { group = "research" }
allow_models = ["anthropic/claude-sonnet-4-5", "openai/gpt-5-mini"]
budget_usd = 200.0
window = "monthly"
```

Startup validation. Each failure refuses to start with a message naming the key,
the same way `OidcConfig::validate_role` does today:

- `entra_group_id` must parse as a GUID. Duplicate ids and duplicate
  `router_group` names across entries are rejected.
- `admin_role` must be empty or in `ADMIN_ROLE_VOCABULARY`.
- `router_group` must not name an existing **manual** group (one with
  `managed_by IS NULL`). Taking over a hand-managed group needs an explicit
  migration step (§8), so that manual members are never silently rewritten.
- `enabled = true` requires `tenant_id` and a resolvable `credential_source`.

## 4. Sync strategy (answers issue question 2)

### 4.1 Options

| Strategy | Freshness | Graph cost | Revokes a leaver who never logs in? | Complexity |
|---|---|---|---|---|
| **A. Invite-time only.** Resolve when an admin provisions the user. | Stale from then on. | Minimal. | **No.** This fails the core requirement. | Low. |
| **B. Per-request check**, cached with a TTL. | TTL-bounded. | One call per user per TTL. The call is on the request's hot path. | Only when they next make a request. Their key stays valid meanwhile. | Medium. It also adds Graph latency and a Graph outage to the proxy path, which the router must not have. |
| **C. Login-time evaluation** (admin OIDC `groups` claim). | Per login. | None, since the groups come from the token. | **No.** API users do not log in; they present keys. | Low, but it only covers admin sessions. |
| **D. Periodic full sync** of each mapped group's members. | `sync_interval_secs`. | O(members) per interval. | **Yes**, within one interval. | Medium. |
| **E. Delta queries** (`/groups/delta` with `members`). | `sync_interval_secs`, with small payloads. | O(changes) per interval. | **Yes**, within one interval. | Higher. It needs a stored delta token and a full resync when the token expires. It returns **direct** members only. |
| **F. Change notifications** (Graph webhooks). | Seconds. | Low. | Yes. | Highest. It needs a public HTTPS callback, subscription renewal, and a fallback poll anyway, because notifications can be lost. |

### 4.2 Recommendation

**D as the correctness backbone, E as an optimisation, C for admin roles.**

- **Periodic full sync (D)** is the source of truth. It is idempotent: each pass
  computes the desired membership set and reconciles against the DB. That makes
  it self-healing after an outage, a missed delta, or a manual edit. At
  router-scale group sizes (tens to low thousands of users) a full
  `transitiveMembers` page walk every 5 minutes costs a few requests.
- **Delta (E)** is a Phase 4 optimisation for large tenants, used only when
  `membership = "direct"`. Delta does not expand nested groups. A full
  reconcile still runs every `N` intervals (e.g. hourly), so a lost or expired
  delta token (`410 Gone` → `syncStateNotFound`) degrades to "run the full sync
  now", never to "assume no changes".
- **Login-time (C)**, used for the admin dashboard only. When an OIDC admin logs
  in, their role is re-derived from mapped groups (a `groups` claim if present,
  otherwise the synced membership table), not taken from the stale
  `auto_provision_role` snapshot. Today a demoted admin stays superadmin
  forever. This closes that.
- **Per-request (B) and webhooks (F) are rejected.** B puts the directory on
  the proxy hot path. F needs inbound public reachability that many deployments
  do not have. Neither is needed once D exists.

The worst-case revocation latency is therefore `sync_interval_secs` (plus Entra's
own replication lag). Operators who need faster revocation lower the interval,
or run `modelrouter directory sync --now` (§9). §12 Q3 asks whether 5 minutes is
the right default.

### 4.3 One sync pass

```
for each [[directory.entra.group]]:
    members = GET {graph_host}/v1.0/groups/{id}/transitiveMembers/microsoft.graph.user
                  ?$select=id,userPrincipalName,mail,displayName,accountEnabled&$top=999
              (follow @odata.nextLink to the end; any non-2xx or parse failure ⇒ the WHOLE pass aborts, §7)
    desired[router_group] = { m.id : m | m.accountEnabled != false }

plan  = diff(desired, current managed memberships in DB)
guard = plan.disables / managed_user_count <= max_disable_fraction      (§7.2)
apply plan in ONE transaction:
    upsert users by external id           (new members: create; renamed: update email/display name)
    open  group_memberships rows          (joined_at = now)
    close group_memberships rows          (disabled_at = now)
    users with zero open managed memberships ⇒ offboard (§5.2)
    write audit_log rows for every change (actor = "directory:entra")
record sync_state: last_success_at, counts, duration
```

Rules:

- Users are keyed by **Entra object id** (`id`), never by UPN or email. Both of
  those can change and can be reassigned to a different person.
- A group that is **absent** from the result (404) or **forbidden** (403) is an
  error, not an empty group. An empty member list is accepted only from a 200
  whose `value` is `[]` and has no `nextLink`. Even then the disable guard applies.
- `accountEnabled = false` counts as "not a member". A blocked Entra account
  loses router access at the next pass, even before anyone removes it from the
  group.
- Guests (`#EXT#` UPNs) are treated like any other member. §12 Q6 asks whether
  that is right.
- Only one sync pass runs at a time. In multi-replica deployments the pass takes
  a DB-level lease (a `directory_sync_state` row with `lease_until`), so that
  replicas do not race to reconcile. This matters on the postgres backend.

## 5. User and key lifecycle

### 5.1 Joining

When a new member appears in a mapped group:

1. Create a `users` row: `name` = UPN (a `-2` style suffix is added on the rare
   collision with a manual user), `email` = `mail` or the UPN, plus
   `external_source = 'entra'` and `external_id = <object id>`.
2. Open a `group_memberships` row for each mapped router group.
3. If a mapping carries a `key` template, issue a key through the same
   `create_api_key` path that the CLI and dashboard use. **The raw key is never
   stored, logged, or put in the audit log.** The raw key has to reach the user
   somehow, so key issuance is **off by default**. The options are in §12 Q1:
   - **(a) Admin hand-off.** The user appears on the dashboard with an "Issue key"
     action that reuses the existing #1 mailto button. This is the default and
     needs no new secret-delivery channel.
   - **(b) Self-service.** An Entra user signs in (the OIDC flow, extended to API
     users) and generates their own key, shown once. This supersedes #1 for
     directory users.
   - (c) Server-sent email. This is rejected for now: it adds an SMTP dependency
     and puts a live secret in a mailbox.

### 5.2 Leaving: revocation (answers "what happens when a user leaves the group")

When a user's **last** open managed membership closes (they left every mapped
group, the account was disabled, or it was deleted):

1. **Every active key for the user is disabled**, through
   `ApiKeyRepository::disable_key`. This is the same call `modelrouter key disable`
   makes, so `enabled = 0` and `disabled_at` is set. It takes effect on the
   **next request**, because `AuthenticatedUser` reads the key from the DB on
   every request and there is no cache to invalidate.
2. The user row is set `enabled = 0` (`UserRepository::set_enabled`). This is
   defence in depth: a key created later by mistake still fails the
   `!user.enabled` check in `src/api/auth.rs`.
3. If the user also holds an `admin_users` row derived from a mapping, its role
   is recomputed (§3.4). Losing every admin-mapped group disables the admin
   row. An existing admin JWT expires within `auth.jwt_expiry_mins`, which is
   the same window a manual disable has today.
4. An audit row records the reason (`left_group`, `account_disabled`,
   `not_found`), the keys disabled, and the sync pass id.

When a user leaves **some** mapped groups but not all, only those memberships
close. Their keys stay live. Their permissions shrink at the next request,
because the auth extractor reloads group names per request.

In-flight streams are not cut. A request that already passed auth completes.
That matches `key disable` today, and §12 Q4 asks whether it is acceptable.

**Re-joining** opens new memberships and re-enables the user row, but **does not
re-enable old keys**. A previously disabled key stays dead, and a fresh key goes
through §5.1. This avoids resurrecting a key that may have been shared or leaked
while its owner was away.

### 5.3 What sync never does

- It never touches users, keys or memberships without `external_source = 'entra'`
  or `managed_by = 'entra'`.
- It never deletes rows. Deleting would break cost-ledger foreign keys and
  history. Offboarding is always a disable.
- It never re-enables a key.

## 6. Promoting the Entra token source to a shared module

The Bing-specific plumbing the issue refers to has already been promoted.
`azure_credentials.rs` and `azure_entra.rs` are shared by `azure`, `foundry` and
`bing_grounding`, behind the provider-neutral `CredentialChain`. The remaining
work is small:

1. **Decouple settings from `ProviderConfig`.** Add
   `AzureSettings::from_parts(label, scope, tenant_id, client_id, client_secret,
   federated_token_file, timeout)`, and make `from_config` a thin wrapper over it.
   The directory module builds its settings from `[directory.entra]` with
   `scope = "{graph_host}/.default"`. The env fallbacks (`AZURE_TENANT_ID`,
   `AZURE_AUTHORITY_HOST`, `IDENTITY_ENDPOINT`, and so on) stay in one place.
2. **Move, don't copy.** Relocate `azure_credentials.rs` and `azure_entra.rs`
   from `src/providers/` to a new `src/azure/` (or `src/identity/entra/`) module,
   and re-export them from `providers` so that existing imports keep compiling.
   There is no behaviour change and no new dependency. This is a pure move
   commit with the existing tests (`azure_credentials/tests.rs`) moving along.
3. **Add a `GRAPH_SCOPE` constant** next to `COGNITIVE_SERVICES_SCOPE`, with
   the documented `https://graph.microsoft.com/.default`, and build the
   sovereign-cloud variants from `graph_host`.
4. **Health.** The directory credential gets a `CredentialReport` in
   `/health/deep`, under the name `directory.entra`, so an expired secret or a
   revoked consent shows up in the health verdict that already exists, not
   only in logs.

A `sovereign cloud` config then needs `AZURE_AUTHORITY_HOST` (already honoured)
plus `graph_host`.

## 7. Offline and failure behaviour

### 7.1 Principle

**A sync pass either applies a complete, validated plan or applies nothing.**
Partial data is never reconciled. A pass that aborts leaves the DB exactly as
it was, so it neither grants nor revokes.

| Failure | Behaviour |
|---|---|
| Token acquisition fails (network, 5xx) | Abort the pass and retry next interval. Health `degraded` once stale (§7.3). |
| Token acquisition fails permanently (`credential_expired`, reauth AADSTS codes) | Abort, `/health/deep` reports `credential_expired` with the remediation text from `CredentialChain`, and log at `error`. |
| Graph 401/403 (consent revoked, permission missing) | Abort the **whole** pass. The error names the missing permission (§10). The result is never read as "the group is empty". |
| Graph 404 for a mapped group | Abort. The error names the `entra_group_id`, since the group was deleted or the id is wrong. |
| Graph 429 / 503 | Honour `Retry-After` within the pass. If the retry budget is exhausted, abort. |
| Paging fails midway | Abort. A half-read member list is never diffed. |
| DB error during apply | Roll back the transaction. The next pass recomputes from scratch. |
| Router starts with Entra unreachable | Serve normally from DB state. Startup **does not** block on Graph. |

**Never grant on a Graph error.** This follows structurally. The only code path
that creates users, opens memberships, or raises an admin role is the `apply`
step of a pass whose every read returned 2xx and parsed. There is no
"default-allow on error" branch to get wrong.

### 7.2 Mass-disable guard

A misconfiguration that reads cleanly can still look like everyone left. Two
examples: a group id pointed at the wrong group, or a nested group removed from
its parent. If a pass would offboard more than `max_disable_fraction` of the
currently managed users (with a floor, e.g. at least 5 users), it:

- applies **no** disables, but still applies joins, since those are not
  dangerous under this guard;
- sets health `degraded` with the count, and writes an audit row;
- waits for an operator to run `modelrouter directory sync --now --confirm-disables`.

The guard never blocks a single-user leave, which is the common case that must be
fast.

### 7.3 Prolonged outage

During an outage the last good state stands, so a leaver keeps access until the
directory can be read again. That is the availability-over-revocation trade.
`on_stale` lets the owner choose:

- `freeze` (**recommended default**): after `stale_after_secs` with no good
  pass, health goes `degraded` and an audit and alert event fires, but no access
  changes.
- `disable_managed`: fail closed. After the stale window, every
  directory-managed user's keys are disabled until a good pass runs.
  Manual users are unaffected. Re-enabling needs fresh keys (§5.2). That makes
  this mode costly, which is why it is not the default. See §12 Q5.

## 8. Migration and backward compatibility

- **Feature off (default):** no behaviour change. The new columns are nullable,
  and the only code change that runs is the `condition.group` and group
  budget-rule enforcement (Phase 1). It applies only where an operator has
  written group-scoped rules. Before Phase 1 ships, no enforced rule referenced a
  group.
  - **Compatibility note:** existing `budget_rules` rows with a `group_name` and
    a non-`target` window **start being enforced** in Phase 1. The migration
    release note must list them. A `modelrouter budget list --group-scoped`
    pre-flight shows which rules will start binding.
- **Schema** (SQLite and `migrations/postgres/` in lockstep, next free number):
  ```sql
  ALTER TABLE users  ADD COLUMN external_source TEXT;   -- NULL = manual
  ALTER TABLE users  ADD COLUMN external_id     TEXT;   -- Entra object id
  CREATE UNIQUE INDEX idx_users_external ON users(external_source, external_id)
      WHERE external_id IS NOT NULL;
  ALTER TABLE groups ADD COLUMN managed_by      TEXT;   -- NULL = manual, 'entra'
  ALTER TABLE groups ADD COLUMN external_id     TEXT;   -- Entra group object id
  ALTER TABLE group_memberships ADD COLUMN source TEXT; -- NULL = manual, 'entra'
  CREATE TABLE directory_sync_state (
      source TEXT PRIMARY KEY, last_success_at TEXT, last_attempt_at TEXT,
      last_error TEXT, delta_token TEXT, lease_until TEXT, stats TEXT NOT NULL DEFAULT '{}'
  );
  ```
- **Manually provisioned users** stay manual forever unless an operator links
  them. `modelrouter directory link --user alice --entra-id <oid>` (or a
  dashboard action) sets `external_*`, after which sync owns the user's
  managed memberships and offboarding. Linking is explicit because
  auto-matching by email would let whoever holds a reassigned mailbox inherit
  someone else's keys and spend history.
- **Auto-link suggestions:** `modelrouter directory link --suggest` lists
  manual users whose email matches a mapped group member's `mail`/UPN. It only
  lists them. An operator confirms each one.
- **Mixed groups:** a manual user can still be added by hand to an
  Entra-managed router group. Their membership row has `source = NULL`, and sync
  never closes it. The dashboard labels which members came from the directory.
- **Existing admin OIDC** is untouched unless a mapping sets `admin_role`. With
  no admin mappings, `auto_provision_role` keeps its current meaning.
- **Turning the feature off** stops the sync. Managed users keep their current
  state and become effectively manual until it is turned back on. Nothing is
  disabled on shutdown.

## 9. Operator surface

- CLI: `modelrouter directory status` (last pass, counts, errors, staleness),
  `directory sync --now [--dry-run] [--confirm-disables]`, `directory link`,
  `directory unlink`. `--dry-run` prints the plan without applying it, and is the
  recommended first step when adding a mapping.
- Dashboard: an Entra badge on managed users and groups, a "Directory" card with
  sync state, and read-only mapping display (mappings live in config).
- `/health/deep`: a `directory.entra` entry with the credential report and sync
  freshness.
- Metrics: `modelrouter_directory_sync_total{result}`,
  `modelrouter_directory_sync_duration_seconds`,
  `modelrouter_directory_users_offboarded_total`.
- Audit: every join, leave, offboard, guard trip and link, with actor
  `directory:entra`.

## 10. Least-privilege Graph permissions

The router uses **application** permissions (client credentials, managed
identity or workload identity; no signed-in user). The requirement is read-only.

| Need | Permission (Application) | Notes |
|---|---|---|
| Read the members (direct or transitive) of mapped groups | `GroupMember.Read.All` | The least-privileged permission Graph documents for listing group members and transitive members. It returns the members' basic profile properties. |
| Read `mail`, `userPrincipalName`, `accountEnabled` of members | `User.ReadBasic.All` if `GroupMember.Read.All` alone does not return `accountEnabled` | **Verify at implementation time** against the current permissions reference. `accountEnabled` is not a basic property in every tenant configuration, so fall back to `User.Read.All` only if it proves necessary, and record why. |
| Delta queries on groups (Phase 4) | `GroupMember.Read.All` | The same grant. |

Explicitly **not** needed: `Group.Read.All` (broader than membership),
`Directory.Read.All`, and any `*.ReadWrite.*`. The router never writes to the
directory.

Scoping further. `GroupMember.Read.All` is tenant-wide. Entra does not support
per-group scoping of application permissions for this API. Deployments that need
to restrict what the router can see can use a dedicated app registration, and
must treat the mapping config as the boundary on what the router acts on. §12 Q7
asks whether that is acceptable.

The admin-login role refresh (§4.2 C) uses the `groups` claim, configured as
"Security groups" or "Groups assigned to the application" on the app
registration. That needs **no** additional Graph permission. On group overage
(more than 200 groups, where the token carries `_claim_names` instead), it falls
back to the synced membership table, never to a live Graph call at login.

## 11. Phased implementation plan and test strategy

Each phase is independently shippable and leaves the tree green. The testing
rule is the repo's: new lines ship covered by `cargo test`, with a target of at
least 80%.

**Phase 1: Groups gate permissions (no Entra).**
- `PolicyConditionConfig.group`; the auth extractor loads active group names into
  `User`; `PolicyEngine::check` enforces non-`target` group budget rules, with the
  most restrictive one winning.
- Tests: extend the `declarative_policy.rs` unit tests (group match and mismatch,
  multi-group, priority). Add `policy.rs` tests for group budget enforcement,
  `target` rules staying unenforced, and stacking with user/key rules. Add an
  integration test through `/v1/chat/completions`: a user in the group is denied
  a model outside the allow-list, and a user outside the group is not.

**Phase 2: Shared Entra module and Graph client.**
- Move the Azure credential modules (§6), add `from_parts` and `GRAPH_SCOPE`,
  and add a minimal `GraphClient` (paging, `Retry-After`, typed errors for
  401/403/404/410).
- Tests: the moved credential tests pass unchanged. A fake-server Graph test
  suite, in the same style as `azure_credentials/tests.rs`, covers multi-page
  results, 429 then success, 403 mid-page (must error, not truncate), 404,
  malformed JSON, and an empty 200.

**Phase 3: Sync and lifecycle.**
- Migration, config parsing and validation, the planner (a pure function:
  desired set plus current DB state gives a plan), the applier (one transaction),
  the mass-disable guard, the lease, and the `directory` CLI.
- Tests:
  - **Planner property tests.** No plan produced from an errored read. A plan
    never touches manual rows. A plan never re-enables a key. Applying the same
    plan twice is a no-op.
  - **Applier tests on SQLite and postgres** (`--features postgres`). A user
    leaving their last group has every key disabled with `disabled_at` set, and
    their next request returns 401. A partial leave keeps keys and shrinks
    permissions. Rejoin does not revive old keys.
  - **Failure-injection tests** with a fake Graph. A 403 on group 2 of 3 changes
    nothing, including for group 1. A token failure changes nothing and sets the
    health verdict. A guard trip applies joins but no disables.
  - **Config validation tests.** A bad GUID, a duplicate mapping, an unknown
    `admin_role`, and a collision with a manual group each refuse to start.
  - **Feature-off test.** With `[directory.entra]` absent, zero outbound HTTP
    (asserted with a fake server that fails on any hit) and an identical policy
    result.

**Phase 4: Admin roles, key issuance, delta.**
- Admin role derivation at OIDC login, and the `key` template with the admin
  hand-off (or self-service, per Q1).
- Delta sync for `membership = "direct"`, with a periodic full reconcile and
  410 recovery.
- Tests: role recompute on login (promotion, demotion, disable), overage
  fallback, delta token persistence, and 410 falling back to a full sync, all
  against the fake Graph.

**Live verification.** All Graph behaviour above is built from Microsoft's
published documentation. As with `azure_credentials.rs`, it is flagged as not
yet exercised against a live tenant until it has been. The README's live-test
checklist gains a section: grant `GroupMember.Read.All` to a test app, map a
test group, add and remove a test user, observe revocation within one interval,
then revoke consent and observe an aborted pass with state unchanged.

## 12. Open questions for the owner

1. **Key delivery.** Is issuing keys on provisioning wanted at all? If so, is it
   admin hand-off via the #1 mailto (the default proposed here) or self-service
   sign-in for API users (a bigger change that supersedes #1 for directory
   users)?
2. **Multi-group semantics.** Most restrictive limit wins (proposed), or most
   permissive, or priority-ordered first match only (the declarative-rule
   behaviour, applied to budget rules too)?
3. **Revocation latency.** Is a 5-minute default interval acceptable, or is
   near-real-time revocation a requirement? The latter would justify Graph change
   notifications (strategy F) and the public callback endpoint it needs.
4. **In-flight requests.** When a key is disabled, is letting already-authorized
   streams finish acceptable (as with `key disable` today), or should
   offboarding cancel them?
5. **Prolonged outage.** `freeze` (proposed default) or `disable_managed`
   (fail closed after `stale_after_secs`)?
6. **Guest accounts.** Should B2B guests in a mapped group be provisioned, or
   excluded by default with an opt-in flag?
7. **Tenant-wide read.** Is `GroupMember.Read.All` (tenant-wide read of group
   membership) acceptable for the router's identity, given there is no per-group
   scoping?
8. **Mapping home.** Config-only (proposed), or should superadmins also edit
   mappings in the dashboard? That would move policy into the DB, and needs an
   answer on precedence when both exist.
9. **Enforcing existing group budget rules.** Phase 1 starts enforcing
   `budget_rules.group_name` rows that are display-only today. Is that
   acceptable behind a release note, or should group enforcement be gated by a
   flag for one release?
10. **Naming.** Should users created from the directory be named by UPN
    (proposed), or by `mailNickname` or display name?

## 13. Alternatives considered

- **SCIM 2.0 inbound provisioning** (Entra pushes users and groups to a
  `/scim/v2` endpoint). Entra runs the sync engine and deprovisioning is
  push-based. It was rejected as the first step: it needs a publicly reachable
  HTTPS endpoint with a bearer secret, a sizeable spec surface (`Users`, `Groups`,
  PATCH semantics, filtering), and Entra's provisioning cycle is itself about 40
  minutes, which is slower than the proposed pull. It remains a reasonable later
  addition for deployments that are publicly reachable, and could reuse the
  planner and applier from Phase 3.
- **App roles instead of groups.** Define app roles on the router's app
  registration, assign groups to roles, and read `appRoleAssignedTo`. This is
  cleaner for admins who think in roles, but it moves the mapping into Entra,
  where it is invisible to router config review, and needs
  `Application.Read.All`. It could be offered as a second membership source
  later.
- **Mapping Entra groups directly to permissions**, with no router-group layer.
  Rejected in §3: it creates a second enforcement path that manual deployments
  never exercise.
