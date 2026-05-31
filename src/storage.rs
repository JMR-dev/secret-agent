use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::models;
use crate::types::{
    ConversationDraft, ConversationRecord, ConversationStatus, ConversationSummary,
    ConversationUpdate, CurrentSettings, ToolKind, UsageAlert,
};

pub struct Store {
    conn: Connection,
}

const GLOBAL_CURRENT_SETTINGS_KEY: &str = "__global__";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open sqlite database {}", path.display()))?;
        conn.pragma_update(None, "foreign_keys", true)
            .context("failed to enable sqlite foreign_keys")?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("failed to enable sqlite WAL mode")?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS conversations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                secretagentconversationId TEXT UNIQUE,
                toolConversationId TEXT,
                tool TEXT NOT NULL,
                model TEXT,
                effort TEXT,
                role_name TEXT,
                role_path TEXT,
                prompt TEXT NOT NULL,
                response TEXT NOT NULL DEFAULT '',
                stderr_output TEXT NOT NULL DEFAULT '',
                extra_args_json TEXT NOT NULL,
                started_at TEXT NOT NULL,
                ended_at TEXT,
                status TEXT NOT NULL,
                usage_alert_json TEXT,
                exit_code INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_conversations_started_at
            ON conversations(started_at DESC);

            CREATE TABLE IF NOT EXISTS current_settings (
                settings_key TEXT PRIMARY KEY,
                role_name TEXT,
                role_path TEXT,
                tool TEXT NOT NULL,
                model TEXT,
                effort TEXT
            );
            "#,
        )?;
        self.ensure_column("conversations", "secretagentconversationId", "TEXT")?;
        self.ensure_column("conversations", "toolConversationId", "TEXT")?;
        self.conn.execute_batch(
            r#"
            CREATE UNIQUE INDEX IF NOT EXISTS idx_conversations_secretagent_id
            ON conversations(secretagentconversationId);
            "#,
        )?;
        self.backfill_secretagent_ids()?;
        self.backfill_tool_conversation_ids()?;
        Ok(())
    }

    pub fn current_settings(
        &self,
        role_name: Option<&str>,
        role_path: Option<&Path>,
    ) -> Result<CurrentSettings> {
        if let Some(settings) = self.current_settings_exact(role_path)? {
            return Ok(settings);
        }
        if role_path.is_some()
            && let Some(settings) = self.current_settings_exact(None)?
        {
            return Ok(settings);
        }

        let _ = role_name;
        Ok(CurrentSettings::default())
    }

    pub fn save_current_settings(
        &self,
        role_name: Option<&str>,
        role_path: Option<&Path>,
        settings: &CurrentSettings,
    ) -> Result<()> {
        models::validate_model_for_tool(settings.tool, settings.model.as_deref())
            .map_err(anyhow::Error::msg)?;
        self.conn.execute(
            r#"
            INSERT INTO current_settings (
                settings_key,
                role_name,
                role_path,
                tool,
                model,
                effort
            ) VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(settings_key) DO UPDATE
            SET role_name = excluded.role_name,
                role_path = excluded.role_path,
                tool = excluded.tool,
                model = excluded.model,
                effort = excluded.effort
            "#,
            params![
                current_settings_key(role_path),
                role_name,
                role_path.map(path_to_string),
                tool_to_storage(settings.tool),
                settings.model.as_deref(),
                settings.effort.as_deref(),
            ],
        )?;
        Ok(())
    }

    pub fn insert_conversation(&self, draft: &ConversationDraft) -> Result<i64> {
        self.conn.execute(
            r#"
            INSERT INTO conversations (
                secretagentconversationId,
                toolConversationId,
                tool,
                model,
                effort,
                role_name,
                role_path,
                prompt,
                response,
                stderr_output,
                extra_args_json,
                started_at,
                ended_at,
                status,
                usage_alert_json,
                exit_code
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, '', '', ?, ?, NULL, ?, NULL, NULL)
            "#,
            params![
                draft.secretagent_conversation_id.as_str(),
                draft.tool_conversation_id.as_deref(),
                draft.tool.to_string().to_lowercase(),
                draft.model.as_deref(),
                draft.effort.as_deref(),
                draft.role_name.as_deref(),
                draft.role_path.as_ref().map(|path| path_to_string(path)),
                draft.prompt.as_str(),
                serde_json::to_string(&draft.extra_args)?,
                timestamp_to_string(draft.started_at),
                draft.status_string(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_tool_conversation_id(&self, id: i64, tool_conversation_id: &str) -> Result<()> {
        self.conn.execute(
            r#"
            UPDATE conversations
            SET toolConversationId = ?
            WHERE id = ?
            "#,
            params![tool_conversation_id, id],
        )?;
        Ok(())
    }

    pub fn finish_conversation(&self, update: &ConversationUpdate) -> Result<()> {
        self.conn.execute(
            r#"
            UPDATE conversations
            SET response = ?,
                stderr_output = ?,
                exit_code = ?,
                ended_at = ?,
                status = ?,
                usage_alert_json = ?,
                toolConversationId = COALESCE(?, toolConversationId)
            WHERE id = ?
            "#,
            params![
                update.response.as_str(),
                update.stderr_output.as_str(),
                update.exit_code,
                timestamp_to_string(update.ended_at),
                update.status.to_string(),
                update
                    .usage_alert
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                update.tool_conversation_id.as_deref(),
                update.id,
            ],
        )?;
        Ok(())
    }

    pub fn list_conversations(&self, limit: usize) -> Result<Vec<ConversationSummary>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT
                id,
                secretagentconversationId,
                toolConversationId,
                tool,
                model,
                effort,
                role_name,
                started_at,
                ended_at,
                status,
                prompt,
                usage_alert_json
            FROM conversations
            ORDER BY datetime(started_at) DESC
            LIMIT ?
            "#,
        )?;

        let rows = stmt.query_map([limit as i64], |row| {
            let prompt: String = row.get(10)?;
            let title = first_line(&prompt);
            Ok(ConversationSummary {
                id: row.get(0)?,
                secretagent_conversation_id: row.get(1)?,
                tool_conversation_id: row.get(2)?,
                tool: parse_tool(row.get::<_, String>(3)?)?,
                model: row.get(4)?,
                effort: row.get(5)?,
                role_name: row.get(6)?,
                started_at: parse_timestamp(row.get::<_, String>(7)?)?,
                ended_at: row
                    .get::<_, Option<String>>(8)?
                    .map(parse_timestamp)
                    .transpose()?,
                status: parse_status(row.get::<_, String>(9)?)?,
                title,
                usage_alert: row
                    .get::<_, Option<String>>(11)?
                    .map(parse_usage_alert)
                    .transpose()?,
            })
        })?;

        let mut conversations = Vec::new();
        for row in rows {
            conversations.push(row?);
        }
        Ok(conversations)
    }

    pub fn get_conversation(&self, id: i64) -> Result<Option<ConversationRecord>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT
                id,
                secretagentconversationId,
                toolConversationId,
                tool,
                model,
                effort,
                role_name,
                role_path,
                started_at,
                ended_at,
                status,
                prompt,
                response,
                stderr_output,
                extra_args_json,
                usage_alert_json,
                exit_code
            FROM conversations
            WHERE id = ?
            "#,
        )?;

        let record = stmt
            .query_row([id], |row| {
                Ok(ConversationRecord {
                    id: row.get(0)?,
                    secretagent_conversation_id: row.get(1)?,
                    tool_conversation_id: row.get(2)?,
                    tool: parse_tool(row.get::<_, String>(3)?)?,
                    model: row.get(4)?,
                    effort: row.get(5)?,
                    role_name: row.get(6)?,
                    role_path: row.get::<_, Option<String>>(7)?.map(PathBuf::from),
                    started_at: parse_timestamp(row.get::<_, String>(8)?)?,
                    ended_at: row
                        .get::<_, Option<String>>(9)?
                        .map(parse_timestamp)
                        .transpose()?,
                    status: parse_status(row.get::<_, String>(10)?)?,
                    prompt: row.get(11)?,
                    response: row.get(12)?,
                    stderr_output: row.get(13)?,
                    extra_args: serde_json::from_str(&row.get::<_, String>(14)?)
                        .unwrap_or_default(),
                    usage_alert: row
                        .get::<_, Option<String>>(15)?
                        .map(parse_usage_alert)
                        .transpose()?,
                    exit_code: row.get(16)?,
                })
            })
            .optional()?;

        Ok(record)
    }
}

impl Store {
    fn current_settings_exact(&self, role_path: Option<&Path>) -> Result<Option<CurrentSettings>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT tool, model, effort
            FROM current_settings
            WHERE settings_key = ?
            "#,
        )?;

        stmt.query_row([current_settings_key(role_path)], |row| {
            Ok(CurrentSettings {
                tool: parse_tool(row.get::<_, String>(0)?)?,
                model: row.get(1)?,
                effort: row.get(2)?,
            })
        })
        .optional()
        .map_err(Into::into)
    }

    fn ensure_column(&self, table: &str, column: &str, definition: &str) -> Result<()> {
        if self.column_exists(table, column)? {
            return Ok(());
        }
        let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {definition}");
        self.conn.execute(&sql, [])?;
        Ok(())
    }

    fn column_exists(&self, table: &str, column: &str) -> Result<bool> {
        let pragma = format!("PRAGMA table_info({table})");
        let mut stmt = self.conn.prepare(&pragma)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn backfill_secretagent_ids(&self) -> Result<()> {
        if self.column_exists("conversations", "secretagent_conversation_id")? {
            self.conn.execute(
                r#"
                UPDATE conversations
                SET secretagentconversationId = COALESCE(secretagentconversationId, secretagent_conversation_id)
                WHERE secretagentconversationId IS NULL
                   OR secretagentconversationId = ''
                "#,
                [],
            )?;
        }

        let mut stmt = self.conn.prepare(
            r#"
            SELECT id
            FROM conversations
            WHERE secretagentconversationId IS NULL
               OR secretagentconversationId = ''
            "#,
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
        for row in rows {
            let id = row?;
            self.conn.execute(
                r#"
                UPDATE conversations
                SET secretagentconversationId = ?
                WHERE id = ?
                "#,
                params![Uuid::now_v7().to_string(), id],
            )?;
        }
        Ok(())
    }

    fn backfill_tool_conversation_ids(&self) -> Result<()> {
        if self.column_exists("conversations", "tool_conversation_id")? {
            self.conn.execute(
                r#"
                UPDATE conversations
                SET toolConversationId = COALESCE(toolConversationId, tool_conversation_id)
                WHERE toolConversationId IS NULL
                   OR toolConversationId = ''
                "#,
                [],
            )?;
        }
        Ok(())
    }
}

impl ConversationDraft {
    fn status_string(&self) -> String {
        ConversationStatus::Running.to_string()
    }
}

fn parse_tool(tool: String) -> rusqlite::Result<ToolKind> {
    match tool.as_str() {
        "claude" => Ok(ToolKind::Claude),
        "agy" => Ok(ToolKind::Agy),
        "codex" => Ok(ToolKind::Codex),
        "copilot" => Ok(ToolKind::Copilot),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            tool.len(),
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown tool kind {tool}"),
            )),
        )),
    }
}

fn tool_to_storage(tool: ToolKind) -> String {
    tool.to_string().to_lowercase()
}

fn parse_status(status: String) -> rusqlite::Result<ConversationStatus> {
    match status.as_str() {
        "running" => Ok(ConversationStatus::Running),
        "completed" => Ok(ConversationStatus::Completed),
        "failed" => Ok(ConversationStatus::Failed),
        "cancelled" => Ok(ConversationStatus::Cancelled),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            status.len(),
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown status {status}"),
            )),
        )),
    }
}

fn parse_timestamp(raw: String) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&raw)
        .map(|ts| ts.with_timezone(&Utc))
        .map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                raw.len(),
                rusqlite::types::Type::Text,
                Box::new(err),
            )
        })
}

fn parse_usage_alert(raw: String) -> rusqlite::Result<UsageAlert> {
    serde_json::from_str(&raw).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            raw.len(),
            rusqlite::types::Type::Text,
            Box::new(err),
        )
    })
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn current_settings_key(role_path: Option<&Path>) -> String {
    role_path
        .map(path_to_string)
        .unwrap_or_else(|| GLOBAL_CURRENT_SETTINGS_KEY.to_string())
}

fn timestamp_to_string(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339()
}

fn first_line(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        "(empty prompt)".to_string()
    } else {
        line.chars().take(80).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::Path;

    use super::Store;
    use crate::types::{CurrentSettings, ToolKind};

    fn temp_db_path(test_name: &str) -> std::path::PathBuf {
        env::temp_dir().join(format!(
            "secret-agent-{test_name}-{}.sqlite",
            uuid::Uuid::now_v7()
        ))
    }

    fn remove_db(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = fs::remove_file(path.with_extension("sqlite-shm"));
    }

    #[test]
    fn current_settings_fall_back_to_global() {
        let path = temp_db_path("settings-fallback");
        let store = Store::open(&path).expect("db should open");
        store
            .save_current_settings(
                None,
                None,
                &CurrentSettings {
                    tool: ToolKind::Codex,
                    model: Some("gpt-5.4".to_string()),
                    effort: Some("high".to_string()),
                },
            )
            .expect("global settings should save");

        let settings = store
            .current_settings(Some("Reviewer"), Some(Path::new("/tmp/reviewer.md")))
            .expect("settings should load");
        assert_eq!(settings.tool, ToolKind::Codex);
        assert_eq!(settings.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(settings.effort.as_deref(), Some("high"));

        drop(store);
        remove_db(&path);
    }

    #[test]
    fn current_settings_are_saved_per_role() {
        let path = temp_db_path("settings-role");
        let store = Store::open(&path).expect("db should open");
        store
            .save_current_settings(
                Some("Reviewer"),
                Some(Path::new("/tmp/reviewer.md")),
                &CurrentSettings {
                    tool: ToolKind::Claude,
                    model: Some("sonnet".to_string()),
                    effort: Some("medium".to_string()),
                },
            )
            .expect("role settings should save");

        let settings = store
            .current_settings(Some("Reviewer"), Some(Path::new("/tmp/reviewer.md")))
            .expect("role settings should load");
        assert_eq!(settings.tool, ToolKind::Claude);
        assert_eq!(settings.model.as_deref(), Some("sonnet"));
        assert_eq!(settings.effort.as_deref(), Some("medium"));

        drop(store);
        remove_db(&path);
    }

    #[test]
    fn invalid_models_are_not_saved() {
        let path = temp_db_path("settings-invalid");
        let store = Store::open(&path).expect("db should open");
        store
            .save_current_settings(
                Some("Reviewer"),
                Some(Path::new("/tmp/reviewer.md")),
                &CurrentSettings {
                    tool: ToolKind::Codex,
                    model: Some("gpt-5.4".to_string()),
                    effort: Some("high".to_string()),
                },
            )
            .expect("initial valid settings should save");

        let error = store
            .save_current_settings(
                Some("Reviewer"),
                Some(Path::new("/tmp/reviewer.md")),
                &CurrentSettings {
                    tool: ToolKind::Codex,
                    model: Some("sonnet".to_string()),
                    effort: Some("high".to_string()),
                },
            )
            .expect_err("invalid settings should fail");
        assert_eq!(
            error.to_string(),
            "Model sonnet not available in this tool. Please re-enter and try again."
        );

        let settings = store
            .current_settings(Some("Reviewer"), Some(Path::new("/tmp/reviewer.md")))
            .expect("settings should still load");
        assert_eq!(settings.tool, ToolKind::Codex);
        assert_eq!(settings.model.as_deref(), Some("gpt-5.4"));

        drop(store);
        remove_db(&path);
    }
}
