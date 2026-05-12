#!/usr/bin/env bash
#
# Sanitize a production Marshall sqlite snapshot for use as a test fixture.
#
# - Replaces all PII / commercial-sensitive fields with deterministic stand-ins
#   keyed by row id, so UNIQUE indexes stay intact.
# - Preserves the special user_map row with linear_user_id = '__system__' as-is
#   (the bot uses it as a sentinel and changing it would break invariants).
# - Schema reference: post-migration 028 (normalize_identities, drop_priority).
#
# Usage:
#   scripts/sanitize-prod-db.sh <path-to-prod.sqlite>
#
# Output: test/fixtures/prod-snapshot.sqlite (overwritten if exists).

set -euo pipefail

PROD_DB="${1:?Usage: $0 <path-to-prod.sqlite>}"
DEST="$(dirname "$0")/../test/fixtures/prod-snapshot.sqlite"

mkdir -p "$(dirname "$DEST")"

# Clean copy with WAL applied (sqlite3 .backup consolidates WAL into single file)
rm -f "$DEST" "${DEST}-wal" "${DEST}-shm"
sqlite3 "$PROD_DB" ".backup '$DEST'"

echo "🧹 Sanitizing tables (preserving __system__ user)..."

sqlite3 "$DEST" <<'SQL'
BEGIN;

-- ---------------------------------------------------------------------------
-- user_map: identity columns + names. The '__system__' row is left untouched
-- because the bot treats it as a sentinel.
-- ---------------------------------------------------------------------------
UPDATE user_map SET
  linear_user_id   = 'lin-user-' || id,
  discord_user_id  = 'disc-' || id,
  discord_username = CASE WHEN discord_username IS NOT NULL THEN 'user' || id ELSE NULL END,
  first_name       = CASE WHEN first_name       IS NOT NULL THEN 'First' || id ELSE NULL END,
  last_name        = CASE WHEN last_name        IS NOT NULL THEN 'Last'  || id ELSE NULL END,
  github_username  = CASE WHEN github_username  IS NOT NULL THEN 'ghuser' || id ELSE NULL END,
  -- timezone reveals approximate user location (Europe/Moscow, Asia/Yerevan, …)
  -- so collapse all real users to UTC.
  timezone         = CASE WHEN timezone         IS NOT NULL THEN 'UTC' ELSE NULL END
WHERE linear_user_id <> '__system__';

-- ---------------------------------------------------------------------------
-- issues: linear identifiers, title, labels (assignee_id is FK → user_map.id,
-- not touched).
-- ---------------------------------------------------------------------------
UPDATE issues SET
  linear_issue_id   = 'issue-' || id,
  linear_identifier = CASE WHEN linear_identifier IS NOT NULL THEN 'TEAM-' || id ELSE NULL END,
  title             = CASE WHEN title             IS NOT NULL THEN 'Issue ' || id ELSE NULL END,
  team_key          = CASE WHEN team_key          IS NOT NULL THEN 'TEAM' ELSE NULL END,
  labels_json       = CASE WHEN labels_json       IS NOT NULL THEN '["later"]' ELSE NULL END;

-- ---------------------------------------------------------------------------
-- requests: dm_channel_id, prompt_text, send_error.
-- ---------------------------------------------------------------------------
UPDATE requests SET
  dm_channel_id = CASE WHEN dm_channel_id IS NOT NULL THEN 'dm-ch-' || id ELSE NULL END,
  prompt_text   = 'sanitized prompt ' || id,
  send_error    = CASE WHEN send_error    IS NOT NULL THEN 'sanitized error' ELSE NULL END;

-- ---------------------------------------------------------------------------
-- replies: free-text fields and Linear comment id (eta_deadline_utc is a
-- timestamp, not PII; left as-is so date checks stay meaningful).
-- ---------------------------------------------------------------------------
UPDATE replies SET
  reply_text  = 'sanitized reply ' || id,
  eta_raw     = CASE WHEN eta_raw    IS NOT NULL THEN '2h'              ELSE NULL END,
  comment_id  = CASE WHEN comment_id IS NOT NULL THEN 'cmt-' || id      ELSE NULL END,
  post_error  = CASE WHEN post_error IS NOT NULL THEN 'sanitized error' ELSE NULL END;

-- ---------------------------------------------------------------------------
-- pipeline_tracking: repo, refs, SHAs, PR metadata, GH usernames, provider
-- run identity. UNIQUE(repo, ref_type, ref_id, head_sha, run_number) and the
-- partial UNIQUE(repo, provider, provider_pipeline_run_id) both stay unique
-- because every component is keyed by id.
-- ---------------------------------------------------------------------------
-- Note: `provider` is intentionally NOT scrubbed. It is a dispatch key the
-- runtime hands to providerRegistry.getByName(...) (valid values:
-- 'woodpecker', 'github_actions', …). Replacing it with a fake string would
-- break every provider-aware code path that touches this row in tests.
UPDATE pipeline_tracking SET
  repo                       = 'repo-' || id,
  ref_id                     = 'ref-' || id,
  head_sha                   = 'sha-' || id,
  head_ref                   = CASE WHEN head_ref                  IS NOT NULL THEN 'head-' || id                       ELSE NULL END,
  pr_url                     = CASE WHEN pr_url                    IS NOT NULL THEN 'https://example.com/pr/' || id     ELSE NULL END,
  pr_title                   = CASE WHEN pr_title                  IS NOT NULL THEN 'PR ' || id                         ELSE NULL END,
  author_github_username     = CASE WHEN author_github_username    IS NOT NULL THEN 'ghuser-' || id                     ELSE NULL END,
  provider_pipeline_run_id   = CASE WHEN provider_pipeline_run_id  IS NOT NULL THEN 'wp-' || id                         ELSE NULL END;

-- ---------------------------------------------------------------------------
-- pipeline_tracking_refs: secondary refs attached to a tracking row.
-- UNIQUE(pipeline_tracking_id, ref_type, ref_id) — ref_id keyed by id keeps it
-- unique within each parent row.
-- ---------------------------------------------------------------------------
UPDATE pipeline_tracking_refs SET
  ref_id                  = 'ref-' || id,
  head_ref                = CASE WHEN head_ref               IS NOT NULL THEN 'head-' || id                   ELSE NULL END,
  pr_url                  = CASE WHEN pr_url                 IS NOT NULL THEN 'https://example.com/pr/' || id ELSE NULL END,
  pr_title                = CASE WHEN pr_title               IS NOT NULL THEN 'PR ' || id                     ELSE NULL END,
  author_github_username  = CASE WHEN author_github_username IS NOT NULL THEN 'ghuser-' || id                 ELSE NULL END;

-- ---------------------------------------------------------------------------
-- review_notifications: UNIQUE(repo, pr_number, reviewer_github_username) —
-- per-id substitution preserves uniqueness.
-- ---------------------------------------------------------------------------
UPDATE review_notifications SET
  repo                      = 'repo-' || id,
  pr_title                  = CASE WHEN pr_title               IS NOT NULL THEN 'PR ' || id                       ELSE NULL END,
  pr_url                    = CASE WHEN pr_url                 IS NOT NULL THEN 'https://example.com/pr/' || id   ELSE NULL END,
  author_github_username    = CASE WHEN author_github_username IS NOT NULL THEN 'ghuser-a-' || id                 ELSE NULL END,
  reviewer_github_username  = 'ghuser-r-' || id;

-- ---------------------------------------------------------------------------
-- review_completed_notifications: UNIQUE(repo, pr_number,
-- reviewer_github_username, review_state) — same approach.
-- ---------------------------------------------------------------------------
UPDATE review_completed_notifications SET
  repo                      = 'repo-' || id,
  pr_title                  = CASE WHEN pr_title IS NOT NULL THEN 'PR ' || id                     ELSE NULL END,
  pr_url                    = CASE WHEN pr_url   IS NOT NULL THEN 'https://example.com/pr/' || id ELSE NULL END,
  reviewer_github_username  = 'ghuser-r-' || id;

-- ---------------------------------------------------------------------------
-- outbox_messages: payload (full Discord/email message text), destination
-- (channel id / email), dedup_key (UNIQUE partial), last_error.
-- ---------------------------------------------------------------------------
UPDATE outbox_messages SET
  destination = 'dest-' || id,
  payload     = '{"sanitized": true}',
  dedup_key   = CASE WHEN dedup_key  IS NOT NULL THEN 'dedup-' || id    ELSE NULL END,
  last_error  = CASE WHEN last_error IS NOT NULL THEN 'sanitized error' ELSE NULL END;

-- ---------------------------------------------------------------------------
-- processed_replies: discord_message_id is the PRIMARY KEY, must stay unique.
-- ---------------------------------------------------------------------------
UPDATE processed_replies SET
  discord_message_id = 'msg-' || rowid;

-- ---------------------------------------------------------------------------
-- ignored_requests: free-text reason.
-- ---------------------------------------------------------------------------
UPDATE ignored_requests SET
  reason = 'sanitized reason';

-- ---------------------------------------------------------------------------
-- reports: payload_json is the full rendered digest, reported_issue_ids_json
-- is a JSON array of Linear issue ids.
-- ---------------------------------------------------------------------------
UPDATE reports SET
  payload_json            = '{"sanitized": true}',
  reported_issue_ids_json = CASE WHEN reported_issue_ids_json IS NOT NULL THEN '[]' ELSE NULL END;

-- ---------------------------------------------------------------------------
-- pipeline_step_notifications: step_name leaks CI workflow internals
-- (e.g. 'ci/woodpecker/pr/<project>'). UNIQUE(pipeline_tracking_id,
-- step_name) stays valid because id is unique within this table.
-- ---------------------------------------------------------------------------
UPDATE pipeline_step_notifications SET
  step_name = 'step-' || id;

-- ---------------------------------------------------------------------------
-- The remaining tables hold no PII or business secrets and need no scrubbing:
--   migrations, team_digests, issue_engagements, pipeline_engagements,
--   morning_reminders.
-- (They contain only ids, FK ids, dates, enum states and counters.)
-- ---------------------------------------------------------------------------

COMMIT;
SQL

# VACUUM cannot run inside a transaction → do it in a separate invocation.
# Then switch back to journal_mode=DELETE so we don't ship -wal/-shm sidecars
# alongside the fixture (the test opens the file via better-sqlite3, which
# defaults to WAL on its own).
sqlite3 "$DEST" "VACUUM; PRAGMA journal_mode=DELETE;" > /dev/null
rm -f "${DEST}-wal" "${DEST}-shm"

# =========================================================================
# Verification: ensure no real PII / commercial data remains.
# Each check counts rows that still don't match the sanitized pattern; any
# non-zero count fails the script and removes the output file.
# =========================================================================
echo "🔍 Verifying sanitization..."
FAIL=0

check() {
  local label="$1" query="$2"
  local count
  count=$(sqlite3 "$DEST" "$query")
  if [ "$count" -gt 0 ]; then
    echo "❌ FAIL: $label — $count rows with unsanitized data"
    FAIL=1
  else
    echo "  ✓ $label"
  fi
}

# user_map (excluding __system__ sentinel row)
check "user_map.linear_user_id" \
  "SELECT COUNT(*) FROM user_map WHERE linear_user_id <> '__system__' AND linear_user_id NOT LIKE 'lin-user-%';"
check "user_map.discord_user_id" \
  "SELECT COUNT(*) FROM user_map WHERE linear_user_id <> '__system__' AND discord_user_id NOT LIKE 'disc-%';"
check "user_map.discord_username" \
  "SELECT COUNT(*) FROM user_map WHERE linear_user_id <> '__system__' AND discord_username IS NOT NULL AND discord_username NOT LIKE 'user%';"
check "user_map.first_name" \
  "SELECT COUNT(*) FROM user_map WHERE linear_user_id <> '__system__' AND first_name IS NOT NULL AND first_name NOT LIKE 'First%';"
check "user_map.last_name" \
  "SELECT COUNT(*) FROM user_map WHERE linear_user_id <> '__system__' AND last_name IS NOT NULL AND last_name NOT LIKE 'Last%';"
check "user_map.github_username" \
  "SELECT COUNT(*) FROM user_map WHERE linear_user_id <> '__system__' AND github_username IS NOT NULL AND github_username NOT LIKE 'ghuser%';"
check "user_map.timezone" \
  "SELECT COUNT(*) FROM user_map WHERE linear_user_id <> '__system__' AND timezone IS NOT NULL AND timezone <> 'UTC';"

# issues
check "issues.linear_issue_id" \
  "SELECT COUNT(*) FROM issues WHERE linear_issue_id NOT LIKE 'issue-%';"
check "issues.linear_identifier" \
  "SELECT COUNT(*) FROM issues WHERE linear_identifier IS NOT NULL AND linear_identifier NOT LIKE 'TEAM-%';"
check "issues.title" \
  "SELECT COUNT(*) FROM issues WHERE title IS NOT NULL AND title NOT LIKE 'Issue %';"
check "issues.team_key" \
  "SELECT COUNT(*) FROM issues WHERE team_key IS NOT NULL AND team_key <> 'TEAM';"
check "issues.labels_json" \
  "SELECT COUNT(*) FROM issues WHERE labels_json IS NOT NULL AND labels_json <> '[\"later\"]';"

# requests
check "requests.dm_channel_id" \
  "SELECT COUNT(*) FROM requests WHERE dm_channel_id IS NOT NULL AND dm_channel_id NOT LIKE 'dm-ch-%';"
check "requests.prompt_text" \
  "SELECT COUNT(*) FROM requests WHERE prompt_text NOT LIKE 'sanitized prompt %';"
check "requests.send_error" \
  "SELECT COUNT(*) FROM requests WHERE send_error IS NOT NULL AND send_error <> 'sanitized error';"

# replies
check "replies.reply_text" \
  "SELECT COUNT(*) FROM replies WHERE reply_text NOT LIKE 'sanitized reply %';"
check "replies.eta_raw" \
  "SELECT COUNT(*) FROM replies WHERE eta_raw IS NOT NULL AND eta_raw <> '2h';"
check "replies.comment_id" \
  "SELECT COUNT(*) FROM replies WHERE comment_id IS NOT NULL AND comment_id NOT LIKE 'cmt-%';"
check "replies.post_error" \
  "SELECT COUNT(*) FROM replies WHERE post_error IS NOT NULL AND post_error <> 'sanitized error';"

# pipeline_tracking
check "pipeline_tracking.repo" \
  "SELECT COUNT(*) FROM pipeline_tracking WHERE repo NOT LIKE 'repo-%';"
check "pipeline_tracking.ref_id" \
  "SELECT COUNT(*) FROM pipeline_tracking WHERE ref_id NOT LIKE 'ref-%';"
check "pipeline_tracking.head_sha" \
  "SELECT COUNT(*) FROM pipeline_tracking WHERE head_sha NOT LIKE 'sha-%';"
check "pipeline_tracking.head_ref" \
  "SELECT COUNT(*) FROM pipeline_tracking WHERE head_ref IS NOT NULL AND head_ref NOT LIKE 'head-%';"
check "pipeline_tracking.pr_url" \
  "SELECT COUNT(*) FROM pipeline_tracking WHERE pr_url IS NOT NULL AND pr_url NOT LIKE 'https://example.com/pr/%';"
check "pipeline_tracking.pr_title" \
  "SELECT COUNT(*) FROM pipeline_tracking WHERE pr_title IS NOT NULL AND pr_title NOT LIKE 'PR %';"
check "pipeline_tracking.author_github_username" \
  "SELECT COUNT(*) FROM pipeline_tracking WHERE author_github_username IS NOT NULL AND author_github_username NOT LIKE 'ghuser-%';"
check "pipeline_tracking.provider_pipeline_run_id" \
  "SELECT COUNT(*) FROM pipeline_tracking WHERE provider_pipeline_run_id IS NOT NULL AND provider_pipeline_run_id NOT LIKE 'wp-%';"

# pipeline_tracking_refs
check "pipeline_tracking_refs.ref_id" \
  "SELECT COUNT(*) FROM pipeline_tracking_refs WHERE ref_id NOT LIKE 'ref-%';"
check "pipeline_tracking_refs.head_ref" \
  "SELECT COUNT(*) FROM pipeline_tracking_refs WHERE head_ref IS NOT NULL AND head_ref NOT LIKE 'head-%';"
check "pipeline_tracking_refs.pr_url" \
  "SELECT COUNT(*) FROM pipeline_tracking_refs WHERE pr_url IS NOT NULL AND pr_url NOT LIKE 'https://example.com/pr/%';"
check "pipeline_tracking_refs.pr_title" \
  "SELECT COUNT(*) FROM pipeline_tracking_refs WHERE pr_title IS NOT NULL AND pr_title NOT LIKE 'PR %';"
check "pipeline_tracking_refs.author_github_username" \
  "SELECT COUNT(*) FROM pipeline_tracking_refs WHERE author_github_username IS NOT NULL AND author_github_username NOT LIKE 'ghuser-%';"

# review_notifications
check "review_notifications.repo" \
  "SELECT COUNT(*) FROM review_notifications WHERE repo NOT LIKE 'repo-%';"
check "review_notifications.pr_title" \
  "SELECT COUNT(*) FROM review_notifications WHERE pr_title IS NOT NULL AND pr_title NOT LIKE 'PR %';"
check "review_notifications.pr_url" \
  "SELECT COUNT(*) FROM review_notifications WHERE pr_url IS NOT NULL AND pr_url NOT LIKE 'https://example.com/pr/%';"
check "review_notifications.author_github_username" \
  "SELECT COUNT(*) FROM review_notifications WHERE author_github_username IS NOT NULL AND author_github_username NOT LIKE 'ghuser-a-%';"
check "review_notifications.reviewer_github_username" \
  "SELECT COUNT(*) FROM review_notifications WHERE reviewer_github_username NOT LIKE 'ghuser-r-%';"

# review_completed_notifications
check "review_completed_notifications.repo" \
  "SELECT COUNT(*) FROM review_completed_notifications WHERE repo NOT LIKE 'repo-%';"
check "review_completed_notifications.pr_title" \
  "SELECT COUNT(*) FROM review_completed_notifications WHERE pr_title IS NOT NULL AND pr_title NOT LIKE 'PR %';"
check "review_completed_notifications.pr_url" \
  "SELECT COUNT(*) FROM review_completed_notifications WHERE pr_url IS NOT NULL AND pr_url NOT LIKE 'https://example.com/pr/%';"
check "review_completed_notifications.reviewer_github_username" \
  "SELECT COUNT(*) FROM review_completed_notifications WHERE reviewer_github_username NOT LIKE 'ghuser-r-%';"

# outbox_messages
check "outbox_messages.destination" \
  "SELECT COUNT(*) FROM outbox_messages WHERE destination NOT LIKE 'dest-%';"
check "outbox_messages.payload" \
  "SELECT COUNT(*) FROM outbox_messages WHERE payload <> '{\"sanitized\": true}';"
check "outbox_messages.dedup_key" \
  "SELECT COUNT(*) FROM outbox_messages WHERE dedup_key IS NOT NULL AND dedup_key NOT LIKE 'dedup-%';"
check "outbox_messages.last_error" \
  "SELECT COUNT(*) FROM outbox_messages WHERE last_error IS NOT NULL AND last_error <> 'sanitized error';"

# pipeline_step_notifications
check "pipeline_step_notifications.step_name" \
  "SELECT COUNT(*) FROM pipeline_step_notifications WHERE step_name NOT LIKE 'step-%';"

# processed_replies
check "processed_replies.discord_message_id" \
  "SELECT COUNT(*) FROM processed_replies WHERE discord_message_id NOT LIKE 'msg-%';"

# ignored_requests
check "ignored_requests.reason" \
  "SELECT COUNT(*) FROM ignored_requests WHERE reason <> 'sanitized reason';"

# reports
check "reports.payload_json" \
  "SELECT COUNT(*) FROM reports WHERE payload_json <> '{\"sanitized\": true}';"
check "reports.reported_issue_ids_json" \
  "SELECT COUNT(*) FROM reports WHERE reported_issue_ids_json IS NOT NULL AND reported_issue_ids_json <> '[]';"

# Foreign key integrity (we never touch FK target ids, but verify anyway)
FK_VIOLATIONS=$(sqlite3 "$DEST" "PRAGMA foreign_key_check;" | wc -l | tr -d ' ')
if [ "$FK_VIOLATIONS" -ne 0 ]; then
  echo "❌ FAIL: foreign_key_check reported $FK_VIOLATIONS violation(s)"
  FAIL=1
else
  echo "  ✓ foreign_key_check"
fi

if [ "$FAIL" -ne 0 ]; then
  echo ""
  echo "🚨 SANITIZATION FAILED — PII may remain. Do NOT commit this file."
  rm -f "$DEST"
  exit 1
fi

echo ""
echo "✅ All checks passed. Sanitized DB saved to $DEST"
