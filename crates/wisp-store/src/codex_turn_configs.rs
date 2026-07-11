use super::{CodexTurnConfigRecord, Store};
use anyhow::Result;
use sqlx::Row;

fn from_row(row: sqlx::sqlite::SqliteRow) -> Result<CodexTurnConfigRecord> {
    Ok(CodexTurnConfigRecord {
        id: row.try_get("id")?,
        frame_id: row.try_get("frame_id")?,
        codex_thread_id: row.try_get("codex_thread_id")?,
        codex_turn_id: row.try_get("codex_turn_id")?,
        mode: row.try_get("mode")?,
        config_version: row.try_get("resolved_config_version")?,
        requested_json: row.try_get("requested_json")?,
        effective_json: row.try_get("effective_json")?,
        actual_json: row.try_get("actual_json")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

impl Store {
    pub async fn save_codex_turn_config(&self, record: &CodexTurnConfigRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO codex_turn_configs(\
             id,frame_id,codex_thread_id,codex_turn_id,mode,config_version,config_version_text,\
             requested_json,effective_json,actual_json,created_at,updated_at) \
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?) \
             ON CONFLICT(id) DO UPDATE SET actual_json=excluded.actual_json, \
             codex_turn_id=excluded.codex_turn_id, updated_at=excluded.updated_at",
        )
        .bind(&record.id)
        .bind(&record.frame_id)
        .bind(record.codex_thread_id.as_deref())
        .bind(record.codex_turn_id.as_deref())
        .bind(&record.mode)
        .bind(0_i64)
        .bind(&record.config_version)
        .bind(&record.requested_json)
        .bind(&record.effective_json)
        .bind(&record.actual_json)
        .bind(record.created_at)
        .bind(record.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update_codex_turn_actual(&self, id: &str, actual_json: &str) -> Result<bool> {
        let result =
            sqlx::query("UPDATE codex_turn_configs SET actual_json=?,updated_at=? WHERE id=?")
                .bind(actual_json)
                .bind(chrono::Utc::now().timestamp())
                .bind(id)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn list_codex_turn_configs(
        &self,
        frame_id: &str,
    ) -> Result<Vec<CodexTurnConfigRecord>> {
        let rows = sqlx::query(
            "SELECT id,frame_id,codex_thread_id,codex_turn_id,mode,\
             CASE WHEN config_version_text<>'' THEN config_version_text ELSE CAST(config_version AS TEXT) END AS resolved_config_version,\
             requested_json,effective_json,actual_json,created_at,updated_at \
             FROM codex_turn_configs WHERE frame_id=? ORDER BY created_at,id",
        )
        .bind(frame_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(from_row).collect()
    }
}
