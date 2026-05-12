#!/bin/bash
# Scrubs PII from a prod database snapshot for E2E testing.
#
# Usage:
#   scripts/scrub-prod-snapshot.sh [source_dir] [dest_file]
#
# Defaults:
#   source_dir = prod/data
#   dest_file  = test/fixtures/prod-snapshot.sqlite
#
# What it does:
#   1. Copies prod DB + WAL/SHM to a temp file
#   2. Checkpoints WAL into the main DB
#   3. Replaces all personal data with fake values (consistent across tables)
#   4. Outputs a single clean .sqlite file
#
# What is scrubbed:
#   - Linear user UUIDs    → 00000000-0000-0000-0000-00000000000N
#   - Discord user IDs     → 10000000000000000N
#   - Linear issue UUIDs   → 11111111-1111-1111-1111-00000000000N
#   - Names                → UserN / TestN
#   - GitHub usernames     → gh-user-N
#   - Titles               → Test issue N
#   - Reply/prompt text    → generic text
#   - DM channel IDs       → cleared
#   - Linear comment IDs   → cleared
#   - PR titles/URLs       → generic
#   - Report payloads      → generic JSON
#
# What is preserved:
#   - Row counts and foreign key relationships
#   - Timestamps (sent_at, created_at, etc.)
#   - States (waiting_reply, state, send_status, etc.)
#   - Migration history
#   - Structural data (team_key, priority, status, labels, etc.)

set -euo pipefail

SRC_DIR="${1:-prod/data}"
SRC_DB="$SRC_DIR/prod-snapshot.sqlite"
DST="${2:-test/fixtures/prod-snapshot.sqlite}"

if [ ! -f "$SRC_DB" ]; then
  echo "ERROR: Source database not found: $SRC_DB"
  exit 1
fi

WORK_DB=$(mktemp /tmp/marshall-scrub-XXXXXX.sqlite)
trap 'rm -f "$WORK_DB" "$WORK_DB-wal" "$WORK_DB-shm"' EXIT

echo "==> Copying source DB to temp..."
cp "$SRC_DB" "$WORK_DB"
[ -f "$SRC_DB-wal" ] && cp "$SRC_DB-wal" "$WORK_DB-wal"
[ -f "$SRC_DB-shm" ] && cp "$SRC_DB-shm" "$WORK_DB-shm"

echo "==> Checkpointing WAL..."
sqlite3 "$WORK_DB" "PRAGMA wal_checkpoint(TRUNCATE);"
rm -f "$WORK_DB-wal" "$WORK_DB-shm"

echo "==> Scrubbing PII..."
sqlite3 "$WORK_DB" <<'SCRUB_SQL'

-- =========================================================================
-- 1. Build consistent ID mappings
-- =========================================================================

-- Linear user IDs (from user_map — the authoritative source)
CREATE TEMP TABLE _linear_user_map AS
  SELECT
    linear_user_id AS real_id,
    printf('00000000-0000-0000-0000-%012d', ROW_NUMBER() OVER (ORDER BY linear_user_id)) AS fake_id
  FROM user_map;

-- Discord user IDs (from user_map)
CREATE TEMP TABLE _discord_user_map AS
  SELECT
    discord_user_id AS real_id,
    printf('%019d', 1000000000000000000 + ROW_NUMBER() OVER (ORDER BY discord_user_id)) AS fake_id
  FROM user_map;

-- Linear issue IDs (from issues table)
CREATE TEMP TABLE _issue_map AS
  SELECT
    linear_issue_id AS real_id,
    printf('11111111-1111-1111-1111-%012d', ROW_NUMBER() OVER (ORDER BY linear_issue_id)) AS fake_id,
    printf('TEST-%d', ROW_NUMBER() OVER (ORDER BY linear_issue_id)) AS fake_identifier
  FROM issues;

-- GitHub usernames (union of all sources)
CREATE TEMP TABLE _github_map AS
  SELECT
    real_name,
    printf('gh-user-%d', ROW_NUMBER() OVER (ORDER BY real_name)) AS fake_name
  FROM (
    SELECT DISTINCT github_username AS real_name FROM user_map WHERE github_username IS NOT NULL
    UNION
    SELECT DISTINCT author_github_username FROM review_notifications
    UNION
    SELECT DISTINCT reviewer_github_username FROM review_notifications
    UNION
    SELECT DISTINCT reviewer_github_username FROM review_completed_notifications
  ) AS all_gh
  WHERE real_name IS NOT NULL AND real_name != '';

-- =========================================================================
-- 2. Scrub user_map
-- =========================================================================

UPDATE user_map SET
  linear_user_id = (SELECT fake_id FROM _linear_user_map WHERE real_id = user_map.linear_user_id),
  discord_user_id = (SELECT fake_id FROM _discord_user_map WHERE real_id = user_map.discord_user_id),
  discord_username = NULL,
  first_name = 'User' || (SELECT ROW_NUMBER() OVER (ORDER BY linear_user_id) FROM user_map u2 WHERE u2.linear_user_id <= user_map.linear_user_id),
  last_name = 'Test' || (SELECT ROW_NUMBER() OVER (ORDER BY linear_user_id) FROM user_map u2 WHERE u2.linear_user_id <= user_map.linear_user_id),
  github_username = (SELECT fake_name FROM _github_map WHERE real_name = user_map.github_username);

-- Fix: user_map names need a simpler approach since the subquery is tricky
-- Re-number after ID replacement
UPDATE user_map SET
  first_name = 'User' || SUBSTR(linear_user_id, -3),
  last_name = 'Test' || SUBSTR(linear_user_id, -3);

-- =========================================================================
-- 3. Scrub issues
-- =========================================================================

UPDATE issues SET
  assignee_linear_user_id = (SELECT fake_id FROM _linear_user_map WHERE real_id = issues.assignee_linear_user_id);

-- Now update issue IDs (must do assignee first since it references user_map)
UPDATE issues SET
  title = 'Test issue ' || (SELECT fake_identifier FROM _issue_map WHERE real_id = issues.linear_issue_id),
  linear_identifier = (SELECT fake_identifier FROM _issue_map WHERE real_id = issues.linear_issue_id),
  linear_issue_id = (SELECT fake_id FROM _issue_map WHERE real_id = issues.linear_issue_id);

-- =========================================================================
-- 4. Scrub requests
-- =========================================================================

UPDATE requests SET
  assignee_linear_user_id = COALESCE(
    (SELECT fake_id FROM _linear_user_map WHERE real_id = requests.assignee_linear_user_id),
    requests.assignee_linear_user_id
  ),
  discord_user_id = COALESCE(
    (SELECT fake_id FROM _discord_user_map WHERE real_id = requests.discord_user_id),
    'disc-unknown'
  ),
  dm_channel_id = NULL,
  prompt_text = 'Test prompt for request ' || id,
  linear_issue_id = COALESCE(
    (SELECT fake_id FROM _issue_map WHERE real_id = requests.linear_issue_id),
    'issue-unknown'
  );

-- =========================================================================
-- 5. Scrub replies
-- =========================================================================

UPDATE replies SET
  discord_user_id = COALESCE(
    (SELECT fake_id FROM _discord_user_map WHERE real_id = replies.discord_user_id),
    'disc-unknown'
  ),
  discord_username = NULL,
  reply_text = 'Test reply ' || id,
  linear_comment_id = NULL,
  eta_raw = CASE WHEN eta_raw IS NOT NULL THEN 'tomorrow' ELSE NULL END;

-- =========================================================================
-- 6. Scrub issue_engagements
-- =========================================================================

UPDATE issue_engagements SET
  discord_user_id = COALESCE(
    (SELECT fake_id FROM _discord_user_map WHERE real_id = issue_engagements.discord_user_id),
    'disc-unknown'
  ),
  assignee_linear_user_id = (SELECT fake_id FROM _linear_user_map WHERE real_id = issue_engagements.assignee_linear_user_id),
  linear_issue_id = COALESCE(
    (SELECT fake_id FROM _issue_map WHERE real_id = issue_engagements.linear_issue_id),
    'issue-unknown'
  );

-- =========================================================================
-- 7. Scrub ignored_requests
-- =========================================================================

UPDATE ignored_requests SET
  discord_user_id = COALESCE(
    (SELECT fake_id FROM _discord_user_map WHERE real_id = ignored_requests.discord_user_id),
    'disc-unknown'
  );

-- =========================================================================
-- 8. Scrub review_notifications
-- =========================================================================

UPDATE review_notifications SET
  reviewer_discord_user_id = COALESCE(
    (SELECT fake_id FROM _discord_user_map WHERE real_id = review_notifications.reviewer_discord_user_id),
    'disc-unknown'
  ),
  author_github_username = COALESCE(
    (SELECT fake_name FROM _github_map WHERE real_name = review_notifications.author_github_username),
    'gh-unknown'
  ),
  reviewer_github_username = COALESCE(
    (SELECT fake_name FROM _github_map WHERE real_name = review_notifications.reviewer_github_username),
    'gh-unknown'
  ),
  pr_title = 'Test PR #' || pr_number,
  pr_url = 'https://github.com/test-org/test-repo/pull/' || pr_number;

-- =========================================================================
-- 9. Scrub review_completed_notifications
-- =========================================================================

UPDATE review_completed_notifications SET
  author_discord_user_id = COALESCE(
    (SELECT fake_id FROM _discord_user_map WHERE real_id = review_completed_notifications.author_discord_user_id),
    'disc-unknown'
  ),
  reviewer_github_username = COALESCE(
    (SELECT fake_name FROM _github_map WHERE real_name = review_completed_notifications.reviewer_github_username),
    'gh-unknown'
  ),
  pr_title = 'Test PR #' || pr_number,
  pr_url = 'https://github.com/test-org/test-repo/pull/' || pr_number;

-- =========================================================================
-- 10. Scrub morning_reminders
-- =========================================================================

UPDATE morning_reminders SET
  discord_user_id = COALESCE(
    (SELECT fake_id FROM _discord_user_map WHERE real_id = morning_reminders.discord_user_id),
    'disc-unknown'
  );

-- =========================================================================
-- 11. Scrub reports (payload may contain names/titles)
-- =========================================================================

UPDATE reports SET
  payload_json = '{"scrubbed": true, "report_kind": "' || report_kind || '"}';

-- =========================================================================
-- 12. Scrub processed_replies (discord message IDs)
-- =========================================================================

UPDATE processed_replies SET
  discord_message_id = 'msg-' || rowid,
  discord_user_id = COALESCE(
    (SELECT fake_id FROM _discord_user_map WHERE real_id = processed_replies.discord_user_id),
    'disc-unknown'
  );

-- =========================================================================
-- 13. Outbox — payloads/destinations/dedup keys may contain message text or IDs
-- =========================================================================

UPDATE outbox_messages SET
  destination = channel || '-dest-' || id,
  dedup_key = CASE WHEN dedup_key IS NOT NULL THEN 'dedup-' || id ELSE NULL END,
  payload = '{"scrubbed": true, "channel": "' || channel || '"}',
  last_error = CASE WHEN last_error IS NOT NULL THEN 'scrubbed error ' || id ELSE NULL END;

-- =========================================================================
-- 14. Pipeline tables — repos are public, scrub only user-facing data
-- =========================================================================

-- pipeline_tracking: repos are public, but author usernames and discord IDs are PII
UPDATE pipeline_tracking SET
  author_github_username = COALESCE(
    (SELECT fake_name FROM _github_map WHERE real_name = pipeline_tracking.author_github_username),
    CASE WHEN author_github_username IS NOT NULL THEN 'gh-unknown' ELSE NULL END
  ),
  author_discord_user_id = COALESCE(
    (SELECT fake_id FROM _discord_user_map WHERE real_id = pipeline_tracking.author_discord_user_id),
    CASE WHEN author_discord_user_id IS NOT NULL THEN 'disc-unknown' ELSE NULL END
  ),
  pr_title = CASE WHEN pr_title IS NOT NULL THEN 'Test PR for pipeline ' || id ELSE NULL END,
  pr_url = CASE WHEN pr_url IS NOT NULL THEN 'https://github.com/test-org/test-repo/pull/' || id ELSE NULL END;

UPDATE pipeline_tracking_refs SET
  author_github_username = COALESCE(
    (SELECT fake_name FROM _github_map WHERE real_name = pipeline_tracking_refs.author_github_username),
    CASE WHEN author_github_username IS NOT NULL THEN 'gh-unknown' ELSE NULL END
  ),
  author_discord_user_id = COALESCE(
    (SELECT fake_id FROM _discord_user_map WHERE real_id = pipeline_tracking_refs.author_discord_user_id),
    CASE WHEN author_discord_user_id IS NOT NULL THEN 'disc-unknown' ELSE NULL END
  ),
  pr_title = CASE WHEN pr_title IS NOT NULL THEN 'Test PR ref ' || id ELSE NULL END,
  pr_url = CASE WHEN pr_url IS NOT NULL THEN 'https://github.com/test-org/test-repo/pull/' || id ELSE NULL END;

-- pipeline_step_notifications: no PII (only pipeline_tracking_id, step_name, notified_at)

-- pipeline_engagements: should be empty, but scrub just in case
DELETE FROM pipeline_engagements;

-- =========================================================================
-- 15. Team digests — no PII (just date + sent_at)
-- =========================================================================
-- Nothing to scrub

-- =========================================================================
-- 16. Vacuum to reclaim space and remove traces of old data
-- =========================================================================

VACUUM;

SCRUB_SQL

echo "==> Copying scrubbed DB to $DST..."
mkdir -p "$(dirname "$DST")"
rm -f "$DST" "$DST-wal" "$DST-shm"
sqlite3 "$WORK_DB" "PRAGMA wal_checkpoint(TRUNCATE);"
sqlite3 "$WORK_DB" ".backup '$DST'"
sqlite3 "$DST" "PRAGMA journal_mode=DELETE;" >/dev/null
rm -f "$DST-wal" "$DST-shm"

echo "==> Verifying..."
echo "  Tables:"
sqlite3 "$DST" ".tables"
echo "  Migrations:"
sqlite3 "$DST" "SELECT COUNT(*) || ' migrations' FROM migrations;"
echo "  Users (sample):"
sqlite3 "$DST" "SELECT linear_user_id, discord_user_id, first_name, last_name FROM user_map LIMIT 3;"
echo "  Issues (sample):"
sqlite3 "$DST" "SELECT linear_issue_id, linear_identifier, title, assignee_linear_user_id FROM issues LIMIT 3;"
echo "  Requests (sample):"
sqlite3 "$DST" "SELECT discord_user_id, prompt_text FROM requests LIMIT 2;"
echo ""
echo "  Row counts:"
for tbl in user_map issues requests replies ignored_requests issue_engagements pipeline_engagements pipeline_tracking pipeline_tracking_refs review_notifications review_completed_notifications morning_reminders reports processed_replies outbox_messages team_digests pipeline_step_notifications; do
  cnt=$(sqlite3 "$DST" "SELECT COUNT(*) FROM $tbl;")
  printf "    %-35s %s\n" "$tbl" "$cnt"
done

echo ""
echo "==> Done. Scrubbed snapshot: $DST"
