-- Schema for webmention.nithitsuki.com comment server.
-- Applied idempotently (CREATE IF NOT EXISTS throughout).

PRAGMA busy_timeout = 5000;
PRAGMA journal_mode = WAL;

CREATE TABLE IF NOT EXISTS comments (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    target_path     TEXT    NOT NULL
                            CHECK (target_path LIKE '/%' AND length(target_path) <= 1024),
    comment_type    TEXT    NOT NULL
                            CHECK (comment_type IN ('native', 'webmention')),
    source_url      TEXT,
    author_name     TEXT    NOT NULL,
    author_url      TEXT,
    author_avatar   TEXT,
    content         TEXT    NOT NULL,
    status          TEXT    NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending', 'approved', 'spam', 'deleted')),
    created_at      TEXT    NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_comments_read
    ON comments(target_path, status, created_at);

-- Fast lookup for webmention idempotency; UNIQUE so INSERT … ON CONFLICT
-- works atomically. SQLite treats NULLs as distinct, so native comments
-- (source_url = NULL) never conflict with each other through this index.
CREATE UNIQUE INDEX IF NOT EXISTS idx_comments_source_target
    ON comments(source_url, target_path)
    WHERE source_url IS NOT NULL;

CREATE TABLE IF NOT EXISTS webmention_seen (
    source          TEXT    NOT NULL,
    target          TEXT    NOT NULL,
    last_seen_at    TEXT    NOT NULL DEFAULT (datetime('now')),
    last_status     TEXT    NOT NULL CHECK (last_status IN ('alive', 'gone')),
    PRIMARY KEY (source, target)
);

CREATE TABLE IF NOT EXISTS github_profiles (
    login       TEXT    PRIMARY KEY,
    name        TEXT,
    avatar_url  TEXT    NOT NULL,
    cached_at   TEXT    NOT NULL DEFAULT (datetime('now')),
    valid       INTEGER NOT NULL DEFAULT 1
);
